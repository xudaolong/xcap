use image::RgbaImage;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDataProvider, CGDirectDisplayID, CGDisplayCreateImage, CGDisplayCreateImageForRect,
    CGImage, CGWindowID, CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
};

use crate::error::{XCapError, XCapResult};

/// 将 CGImage 的像素数据转换为 RGBA RgbaImage。
/// 兼容行尾 padding 的 BGRA/其它 32bit 布局（统一按 BGRA 处理并交换到 RGBA）。
fn cg_image_to_rgba(cg_image: Option<&CGImage>) -> XCapResult<RgbaImage> {
    let width = CGImage::width(cg_image);
    let height = CGImage::height(cg_image);
    let bytes_per_row = CGImage::bytes_per_row(cg_image);
    let data_provider = CGImage::data_provider(cg_image);

    let data = CGDataProvider::data(data_provider.as_deref())
        .ok_or_else(|| XCapError::new("Failed to copy data"))?;

    if width == 0 || height == 0 {
        return Err(XCapError::new("Empty CGImage"));
    }

    let len = width * height * 4;
    let mut buffer: Vec<u8> = Vec::with_capacity(len);
    // SAFETY: `buffer` has capacity for `len` bytes, and every byte is
    // initialized by the conversion loop below before `set_len` is called.
    let dst = unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), len) };

    // SAFETY: the returned CFData is immutable for the duration of this read,
    // and all indexing stays within its length.
    let src = unsafe { data.as_bytes_unchecked() };

    // Some platforms e.g. MacOS can have extra bytes at the end of each row.
    // See
    // https://github.com/nashaofu/xcap/issues/29
    // https://github.com/nashaofu/xcap/issues/38
    //
    // Strip row padding and convert BGRA to RGBA in a single pass.
    if bytes_per_row == width * 4 {
        bgra_to_rgba(&src[..len], dst);
    } else {
        for (src_row, dst_row) in src
            .chunks_exact(bytes_per_row)
            .zip(dst.chunks_exact_mut(width * 4))
        {
            bgra_to_rgba(&src_row[..width * 4], dst_row);
        }
    }

    // SAFETY: all `len` bytes of `buffer` were written above.
    unsafe { buffer.set_len(len) };

    RgbaImage::from_raw(width as u32, height as u32, buffer)
        .ok_or_else(|| XCapError::new("RgbaImage::from_raw failed"))
}

/// 截取指定窗口列表区域的合成画面（旧路径，保留给窗口截屏等场景）
pub fn capture(
    cg_rect: CGRect,
    list_option: CGWindowListOption,
    window_id: CGWindowID,
    image_option: CGWindowImageOption,
) -> XCapResult<RgbaImage> {
    let cg_image = CGWindowListCreateImage(cg_rect, list_option, window_id, image_option);

    cg_image_to_rgba(cg_image.as_deref())
}

/// 直接读取显示器的当前帧缓冲（快速截屏路径）。
///
/// 相比 `CGWindowListCreateImage`（需枚举窗口并合成，冷启动数百毫秒），
/// `CGDisplayCreateImage` 直接抓取该显示器的当前画面（典型 20-80ms），
/// 语义即为「调用瞬间的画面」，不含鼠标光标——符合截屏选区背景的需求。
pub fn capture_display(display_id: CGDirectDisplayID) -> XCapResult<RgbaImage> {
    let cg_image = CGDisplayCreateImage(display_id)
        .ok_or_else(|| XCapError::new("CGDisplayCreateImage failed"))?;

    cg_image_to_rgba(Some(cg_image.as_ref()))
}

/// 直接读取显示器指定区域的当前帧缓冲（快速截屏路径）。
/// `rect` 需为显示器全局坐标系（同 CGDisplayBounds）。
pub fn capture_display_rect(display_id: CGDirectDisplayID, rect: CGRect) -> XCapResult<RgbaImage> {
    let cg_image = CGDisplayCreateImageForRect(display_id, rect)
        .ok_or_else(|| XCapError::new("CGDisplayCreateImageForRect failed"))?;

    cg_image_to_rgba(Some(cg_image.as_ref()))
}

/// Convert BGRA pixels to RGBA. Written with u32 lane ops so LLVM can
/// auto-vectorize it into SIMD byte shuffles.
#[inline]
fn bgra_to_rgba(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let px = u32::from_ne_bytes([s[0], s[1], s[2], s[3]]);
        let px = (px & 0xff00_ff00) | ((px & 0x00ff_0000) >> 16) | ((px & 0x0000_00ff) << 16);
        d.copy_from_slice(&px.to_ne_bytes());
    }
}