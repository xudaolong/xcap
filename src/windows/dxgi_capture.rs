//! Windows 单帧快速截屏：DXGI Desktop Duplication（GPU 直读桌面帧）。
//!
//! 语义：**热键瞬间的真实桌面**。故意不做 warm capturer / 不复用 OutputDuplication /
//! 不缓存上一帧像素——那些路径曾导致画面不精准（stale）。
//!
//! 可安全复用的只有「无画面语义」的基础设施：
//! - `ID3D11Device` / `ID3D11DeviceContext`（创建设备很贵）
//! - 同尺寸 `STAGING` 纹理（每次仍从**本次** DuplicateOutput 的新帧 Copy）
//!
//! 每次截屏仍：`DuplicateOutput` → 等到桌面 present → staging copy → BGRA→RGBA。

use std::sync::Mutex;
use std::time::Instant;

use windows::{
    Win32::{
        Graphics::{
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
                ID3D11Resource, ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
                DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

use image::RgbaImage;

use crate::{XCapError, XCapResult};

use super::utils::{bgra_to_rgba, create_d3d_device};

/// 单次 Acquire 超时（毫秒）。多轮合计约 1s。
const ACQUIRE_TIMEOUT_MS: u32 = 125;
/// 超时 / 指针-only / 无 resource 时的最大重试次数。
const ACQUIRE_MAX_ATTEMPTS: u32 = 8;

struct SharedDxgi {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// 仅缓存 staging 尺寸与句柄；内容每次被新帧覆盖，不是 warm 桌面帧
    staging: Option<(u32, u32, ID3D11Texture2D)>,
}

static SHARED_DXGI: Mutex<Option<SharedDxgi>> = Mutex::new(None);

fn create_shared_dxgi() -> XCapResult<SharedDxgi> {
    let device = create_d3d_device(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
    )?;
    let context = unsafe { device.GetImmediateContext()? };
    Ok(SharedDxgi {
        device,
        context,
        staging: None,
    })
}

/// 应用启动时可调用：只创建 D3D 设备，不截屏、不 DuplicateOutput（无 stale 风险）。
pub fn preheat_dxgi_device() {
    let t0 = Instant::now();
    match SHARED_DXGI.lock() {
        Ok(mut guard) => {
            if guard.is_none() {
                match create_shared_dxgi() {
                    Ok(shared) => {
                        *guard = Some(shared);
                        log::info!("[xcap] DXGI device preheated in {:?}", t0.elapsed());
                    }
                    Err(e) => log::warn!("[xcap] DXGI device preheat failed: {}", e),
                }
            }
        }
        Err(e) => log::warn!("[xcap] DXGI shared lock poisoned on preheat: {}", e),
    }
}

fn reset_shared_dxgi(guard: &mut Option<SharedDxgi>) {
    *guard = None;
}

fn is_recreate_error(code: windows::core::HRESULT) -> bool {
    code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
}

fn looks_like_device_lost(err: &XCapError) -> bool {
    let msg = err.to_string();
    msg.contains("DEVICE_REMOVED") || msg.contains("ACCESS_LOST") || msg.contains("DEVICE_RESET")
}

/// 用 DXGI 抓取指定显示器的一帧（按 h_monitor 匹配 output）。
/// `x/y/width/height` 为该显示器内的区域（0 起始，物理像素）。
pub(super) fn capture_monitor(
    h_monitor: HMONITOR,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> XCapResult<RgbaImage> {
    let total = Instant::now();
    match capture_monitor_once(h_monitor, x, y, width, height) {
        Ok(img) => {
            log::info!("[xcap] DXGI capture_monitor done in {:?}", total.elapsed());
            Ok(img)
        }
        Err(e) if looks_like_device_lost(&e) => {
            log::warn!("[xcap] DXGI device lost, recreating: {}", e);
            if let Ok(mut guard) = SHARED_DXGI.lock() {
                reset_shared_dxgi(&mut guard);
            }
            let img = capture_monitor_once(h_monitor, x, y, width, height)?;
            log::info!(
                "[xcap] DXGI capture_monitor done (after recreate) in {:?}",
                total.elapsed()
            );
            Ok(img)
        }
        Err(e) => Err(e),
    }
}

fn capture_monitor_once(
    h_monitor: HMONITOR,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> XCapResult<RgbaImage> {
    let mut guard = SHARED_DXGI
        .lock()
        .map_err(|e| XCapError::new(format!("DXGI shared lock poisoned: {}", e)))?;

    if guard.is_none() {
        let t0 = Instant::now();
        *guard = Some(create_shared_dxgi()?);
        log::debug!("[xcap] DXGI create device: {:?}", t0.elapsed());
    }

    let shared = guard
        .as_mut()
        .ok_or_else(|| XCapError::new("DXGI shared device missing"))?;

    unsafe {
        let dxgi_device = match shared.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                reset_shared_dxgi(&mut *guard);
                return Err(XCapError::new(format!("cast IDXGIDevice: {e:?}")));
            }
        };

        let adapter = match dxgi_device.GetAdapter() {
            Ok(a) => a,
            Err(e) => {
                if is_recreate_error(e.code()) {
                    reset_shared_dxgi(&mut *guard);
                }
                return Err(XCapError::new(format!("GetAdapter failed: {e:?}")));
            }
        };

        let mut output_index: u32 = 0;
        loop {
            let output = match adapter.EnumOutputs(output_index) {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => {
                    return Err(XCapError::new(format!(
                        "DXGI output not found for h_monitor {:?}",
                        h_monitor
                    )));
                }
                Err(e) => return Err(XCapError::new(format!("EnumOutputs failed: {e:?}"))),
            };
            output_index += 1;

            let output_desc = output.GetDesc()?;
            if output_desc.Monitor != h_monitor {
                continue;
            }

            let output1 = output.cast::<IDXGIOutput1>()?;

            // 关键：每次新建 duplication，保证是「此刻」的桌面，不做 warm 复用
            let t_dup = Instant::now();
            let duplication = match output1.DuplicateOutput(&dxgi_device) {
                Ok(d) => d,
                Err(e) => {
                    if is_recreate_error(e.code()) {
                        reset_shared_dxgi(&mut *guard);
                    }
                    return Err(XCapError::new(format!("DuplicateOutput failed: {e:?}")));
                }
            };
            log::debug!("[xcap] DXGI DuplicateOutput: {:?}", t_dup.elapsed());

            return capture_one_frame(shared, &duplication, x, y, width, height);
        }
    }
}

#[inline]
fn is_desktop_present_frame(info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
    info.LastPresentTime != 0 || info.AccumulatedFrames > 0
}

fn ensure_staging(
    shared: &mut SharedDxgi,
    width: u32,
    height: u32,
    template: &D3D11_TEXTURE2D_DESC,
) -> XCapResult<ID3D11Texture2D> {
    if let Some((w, h, tex)) = &shared.staging {
        if *w == width && *h == height {
            return Ok(tex.clone());
        }
    }

    let t0 = Instant::now();
    let mut staging_desc = *template;
    staging_desc.Width = width;
    staging_desc.Height = height;
    staging_desc.BindFlags = 0;
    staging_desc.MiscFlags = 0;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;

    let staging = unsafe {
        let mut staging = None;
        shared
            .device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        staging.ok_or(XCapError::new("CreateTexture2D staging failed"))?
    };
    log::debug!(
        "[xcap] DXGI create staging {}x{}: {:?}",
        width,
        height,
        t0.elapsed()
    );
    shared.staging = Some((width, height, staging.clone()));
    Ok(staging)
}

fn capture_one_frame(
    shared: &mut SharedDxgi,
    duplication: &IDXGIOutputDuplication,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> XCapResult<RgbaImage> {
    unsafe {
        for attempt in 0..ACQUIRE_MAX_ATTEMPTS {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let t_acq = Instant::now();
            match duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut frame_info, &mut resource) {
                Ok(()) => {
                    log::debug!(
                        "[xcap] DXGI AcquireNextFrame ok in {:?} (attempt={})",
                        t_acq.elapsed(),
                        attempt
                    );
                }
                Err(e) => {
                    let _ = duplication.ReleaseFrame();
                    if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                        log::debug!(
                            "[xcap] DXGI AcquireNextFrame timeout (attempt={})",
                            attempt
                        );
                        continue;
                    }
                    if is_recreate_error(e.code()) {
                        return Err(XCapError::new(format!(
                            "AcquireNextFrame DEVICE_REMOVED/ACCESS_LOST: {e:?}"
                        )));
                    }
                    return Err(XCapError::new(format!("AcquireNextFrame failed: {e:?}")));
                }
            }

            if !is_desktop_present_frame(&frame_info) {
                log::debug!(
                    "[xcap] DXGI skip pointer-only frame (attempt={}, AccumulatedFrames={})",
                    attempt,
                    frame_info.AccumulatedFrames
                );
                let _ = duplication.ReleaseFrame();
                continue;
            }

            let Some(resource) = resource else {
                log::debug!(
                    "[xcap] DXGI present frame without resource (attempt={}), retry",
                    attempt
                );
                let _ = duplication.ReleaseFrame();
                continue;
            };

            let result = (|| -> XCapResult<RgbaImage> {
                let source_texture = resource.cast::<ID3D11Texture2D>()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                source_texture.GetDesc(&mut desc);

                let (x, y, width, height) = (
                    x.min(desc.Width),
                    y.min(desc.Height),
                    width.min(desc.Width.saturating_sub(x)),
                    height.min(desc.Height.saturating_sub(y)),
                );
                if width == 0 || height == 0 {
                    return Err(XCapError::new("DXGI capture region is empty"));
                }

                let staging = ensure_staging(shared, width, height, &desc)?;

                let t_copy = Instant::now();
                let region = D3D11_BOX {
                    left: x,
                    top: y,
                    right: x + width,
                    bottom: y + height,
                    front: 0,
                    back: 1,
                };
                shared.context.CopySubresourceRegion(
                    Some(&staging.cast()?),
                    0,
                    0,
                    0,
                    0,
                    Some(&source_texture.cast()?),
                    0,
                    Some(&region),
                );

                let mapped_resource: ID3D11Resource = staging.cast()?;
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                shared.context.Map(
                    Some(&mapped_resource.clone()),
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )?;

                let mut bgra = vec![0u8; (width * height * 4) as usize];
                let src_ptr = mapped.pData as *const u8;
                for row in 0..height {
                    let src_offset = (row * mapped.RowPitch) as usize;
                    let dst_offset = (row * width * 4) as usize;
                    let src_slice =
                        std::slice::from_raw_parts(src_ptr.add(src_offset), (width * 4) as usize);
                    bgra[dst_offset..dst_offset + (width * 4) as usize].copy_from_slice(src_slice);
                }
                shared.context.Unmap(Some(&mapped_resource), 0);
                log::debug!(
                    "[xcap] DXGI staging copy+map {}x{}: {:?}",
                    width,
                    height,
                    t_copy.elapsed()
                );

                let t_conv = Instant::now();
                let rgba = bgra_to_rgba(bgra);
                log::debug!("[xcap] DXGI BGRA→RGBA: {:?}", t_conv.elapsed());

                RgbaImage::from_raw(width, height, rgba)
                    .ok_or_else(|| XCapError::new("RgbaImage::from_raw failed"))
            })();

            let _ = duplication.ReleaseFrame();
            return result;
        }

        Err(XCapError::new(
            "DXGI acquire frame failed: no desktop present after retries",
        ))
    }
}
