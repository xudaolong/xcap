//! Windows 单帧快速截屏：DXGI Desktop Duplication（GPU 直读桌面帧）。
//!
//! 相比 GDI BitBlt（全屏 GetWindowDC + BitBlt + 逐行拷贝，4K 下 50ms+），
//! DXGI 通过 `IDXGIOutputDuplication` 直接取 GPU 上的桌面帧，再 staging 拷贝
//! 回 CPU，语义为「调用瞬间的画面」（建立 duplication 后首帧即当前桌面）。
//!
//! 单帧路径每次新建 duplication（简单可靠，无设备/热插拔状态管理）；
//! `AcquireNextFrame` 取帧失败由调用方 fallback 到 GDI。
//!
//! # `LastPresentTime == 0` 处理（单帧截屏最佳实践）
//!
//! 按 MSDN `DXGI_OUTDUPL_FRAME_INFO`：`LastPresentTime == 0` 表示自上次
//! `AcquireNextFrame` 以来**桌面位图未发生新的 present**（常见于仅鼠标
//! 形状/位置更新）。此时若仍返回了 `DesktopResource`，其中仍是当前完整
//! 桌面纹理，单帧截屏应直接使用，而**不能**当作失败回退 GDI。
//!
//! 仅在超时或 `resource` 为空时重试；耗尽预算后再由上层 fallback。

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

/// 单次 Acquire 超时（毫秒）。多轮合计约 1s，与旧实现总预算一致。
const ACQUIRE_TIMEOUT_MS: u32 = 125;
/// 超时 / 无 resource 时的最大重试次数。
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

/// 等待并取回一帧。有有效 DesktopResource 即用于单帧截屏（含指针-only 更新）。
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
                    // 失败也要尝试 Release，避免 duplication 卡在 acquired 状态
                    let _ = duplication.ReleaseFrame();
                    if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                        continue;
                    }
                    return Err(XCapError::new(format!("AcquireNextFrame failed: {e:?}")));
                }
            }

            // 无纹理：多为指针元数据通知，必须 Release 后再 Acquire
            let Some(resource) = resource else {
                let _ = duplication.ReleaseFrame();
                continue;
            };

            // LastPresentTime == 0：桌面位图相对「上次 Acquire」无新 present（常为鼠标更新），
            // 但 resource 仍是当前桌面，单帧截屏直接用。
            if frame_info.LastPresentTime == 0 {
                log::debug!(
                    "[xcap] DXGI pointer-only/no-new-present frame accepted for screenshot (attempt={})",
                    attempt
                );
            }

            let result = (|| -> XCapResult<RgbaImage> {
                let source_texture = resource.cast::<ID3D11Texture2D>()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                source_texture.GetDesc(&mut desc);

                // 越界保护：请求区域超出桌面帧时收缩到帧内
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
            "DXGI acquire frame failed: no desktop resource after retries",
        ))
    }
}
