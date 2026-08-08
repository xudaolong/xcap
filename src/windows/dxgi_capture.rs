//! Windows 单帧快速截屏：DXGI Desktop Duplication（GPU 直读桌面帧）。
//!
//! 相比 GDI BitBlt（全屏 GetWindowDC + BitBlt + 逐行拷贝，4K 下 50ms+），
//! DXGI 通过 `IDXGIOutputDuplication` 直接取 GPU 上的桌面帧，再 staging 拷贝
//! 回 CPU，语义为「调用瞬间的画面」。
//!
//! 单帧路径每次新建 duplication（简单可靠，无设备/热插拔状态管理）；
//! 取帧失败由调用方 fallback 到 GDI。
//!
//! # 新建 duplication 后的首帧语义
//!
//! 按 MSDN：`LastPresentTime == 0` 且 `AccumulatedFrames == 0` 表示本次仅有
//! 鼠标形状/位置更新，**桌面位图未 present**。在**刚 DuplicateOutput** 时若直接
//! 使用该帧的 `DesktopResource`，实测可能得到近乎空白纹理（LZ4 异常高压缩比）。
//!
//! 因此单帧截屏必须等到一次真正的桌面 present：
//! `LastPresentTime != 0` 或 `AccumulatedFrames > 0`，再 `texture_to_frame`。
//! 指针-only / 超时则 Release 后重试；耗尽预算再交给上层 GDI。

use windows::{
    Win32::{
        Graphics::{
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED,
                D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
                IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

use image::RgbaImage;

use crate::{XCapError, XCapResult};

use super::utils::{create_d3d_device, texture_to_frame};

/// 单次 Acquire 超时（毫秒）。多轮合计约 1s。
const ACQUIRE_TIMEOUT_MS: u32 = 125;
/// 超时 / 指针-only / 无 resource 时的最大重试次数。
const ACQUIRE_MAX_ATTEMPTS: u32 = 8;

/// 用 DXGI 抓取指定显示器的一帧（按 h_monitor 匹配 output）。
/// `x/y/width/height` 为该显示器内的区域（0 起始，物理像素）。
pub(super) fn capture_monitor(
    h_monitor: HMONITOR,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> XCapResult<RgbaImage> {
    unsafe {
        let d3d_device = create_d3d_device(
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
        )?;
        let dxgi_device = d3d_device.cast::<IDXGIDevice>()?;
        let d3d_context = d3d_device.GetImmediateContext()?;

        let adapter = dxgi_device.GetAdapter()?;

        // 枚举 adapter 的输出，找到与目标 HMONITOR 匹配的那块屏
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
            let duplication = output1.DuplicateOutput(&dxgi_device)?;

            // capture_one_frame 内部负责 ReleaseFrame，避免双释放
            return capture_one_frame(
                &d3d_device,
                &d3d_context,
                &duplication,
                x,
                y,
                width,
                height,
            );
        }
    }
}

/// 是否为可用的桌面 present 帧（非指针-only）。
#[inline]
fn is_desktop_present_frame(info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
    info.LastPresentTime != 0 || info.AccumulatedFrames > 0
}

/// 等待真正的桌面 present 帧并取回像素。
fn capture_one_frame(
    d3d_device: &ID3D11Device,
    d3d_context: &ID3D11DeviceContext,
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

            match duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut frame_info, &mut resource) {
                Ok(()) => {}
                Err(e) => {
                    let _ = duplication.ReleaseFrame();
                    if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                        log::debug!(
                            "[xcap] DXGI AcquireNextFrame timeout (attempt={})",
                            attempt
                        );
                        continue;
                    }
                    return Err(XCapError::new(format!("AcquireNextFrame failed: {e:?}")));
                }
            }

            // 新建 duplication 后常见首包为指针-only：必须丢弃并重试，否则可能读到空白纹理
            if !is_desktop_present_frame(&frame_info) {
                log::debug!(
                    "[xcap] DXGI skip pointer-only frame (attempt={}, LastPresentTime=0, AccumulatedFrames={})",
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

            log::debug!(
                "[xcap] DXGI desktop present frame (attempt={}, AccumulatedFrames={}, LastPresentTime!=0={})",
                attempt,
                frame_info.AccumulatedFrames,
                frame_info.LastPresentTime != 0
            );

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

                let frame = texture_to_frame(
                    d3d_device,
                    d3d_context,
                    &source_texture,
                    x,
                    y,
                    width,
                    height,
                )?;

                RgbaImage::from_raw(frame.width, frame.height, frame.raw)
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
