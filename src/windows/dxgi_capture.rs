//! Windows 单帧快速截屏：DXGI Desktop Duplication（GPU 直读桌面帧）。
//!
//! 相比 GDI BitBlt（全屏 GetWindowDC + BitBlt + 逐行拷贝，4K 下 50ms+），
//! DXGI 通过 `IDXGIOutputDuplication` 直接取 GPU 上的桌面帧，再 staging 拷贝
//! 回 CPU，语义为「调用瞬间的画面」（建立 duplication 后首帧即当前桌面）。
//!
//! 单帧路径每次新建 duplication（简单可靠，无设备/热插拔状态管理）；
//! `AcquireNextFrame` 等待首帧（1s 超时，正常立即返回）。失败由调用方
//! fallback 到 GDI。

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

use crate::{
    XCapError, XCapResult,
    video_recorder::Frame,
};

use super::utils::{create_d3d_device, texture_to_frame};

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

            let image = capture_one_frame(
                &d3d_device,
                &d3d_context,
                &duplication,
                x,
                y,
                width,
                height,
            )?;
            let _ = duplication.ReleaseFrame();
            return Ok(image);
        }
    }
}

/// 等待并取回一帧。建立 duplication 后首帧即当前桌面内容。
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
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // 1s 超时等首帧（正常情况建立 duplication 后立即可得）
        match duplication.AcquireNextFrame(1000, &mut frame_info, &mut resource) {
            Ok(()) => {}
            Err(e) => {
                let _ = duplication.ReleaseFrame();
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Err(XCapError::new("DXGI acquire frame timeout"));
                }
                return Err(XCapError::new(format!("AcquireNextFrame failed: {e:?}")));
            }
        }

        if frame_info.LastPresentTime == 0 {
            return Err(XCapError::new("DXGI no new frame presented"));
        }

        let resource = resource.ok_or(XCapError::new("AcquireNextFrame returned no resource"))?;
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
    }
}