use std::sync::OnceLock;
use std::time::{Duration, Instant};

use block2::RcBlock;
use image::RgbaImage;
use libc::{RTLD_LAZY, dlopen};
use objc2::{msg_send, runtime::AnyClass, sel};
use rayon::prelude::*;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDataProvider, CGDirectDisplayID, CGDisplayBounds, CGDisplayCreateImage,
    CGDisplayCreateImageForRect, CGImage, CGMainDisplayID, CGWindowID, CGWindowImageOption,
    CGWindowListCreateImage, CGWindowListOption,
};
use objc2_foundation::NSError;

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
    // 两种布局都按行/块无依赖拆分，rayon 并行（4K 屏 42.7MB 单线程 ~14ms，
    // 并行后 ~5ms；小图 rayon 开销可忽略）。
    if bytes_per_row == width * 4 {
        const CHUNK: usize = 1 << 20; // 1MB，4 的倍数，块边界不会切开像素
        dst.par_chunks_mut(CHUNK)
            .zip(src[..len].par_chunks(CHUNK))
            .for_each(|(dst_chunk, src_chunk)| bgra_to_rgba(src_chunk, dst_chunk));
    } else {
        dst.par_chunks_mut(width * 4)
            .zip(src.par_chunks(bytes_per_row))
            .for_each(|(dst_row, src_row)| bgra_to_rgba(&src_row[..width * 4], dst_row));
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

/// macOS 14+ 快速路径：ScreenCaptureKit `SCScreenshotManager.captureImageInRect:`。
///
/// 通过 `dlopen` + 运行时类/方法检查懒加载 ScreenCaptureKit，不引入链接期依赖，
/// 因此在 macOS < 12.3（无框架）和 12.3–13.x（无 `SCScreenshotManager` 类）上
/// 都安全返回 `None`，由调用方回退 `CGDisplayCreateImage`。
/// 像素转换（BGRA→RGBA）在 completion 回调线程内完成，避免额外的 retain/拷贝。
fn capture_display_sck(display_id: CGDirectDisplayID) -> Option<RgbaImage> {
    static MANAGER: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
    let cls = (*MANAGER.get_or_init(|| {
        if let Some(cls) = AnyClass::get(c"SCScreenshotManager") {
            return Some(cls);
        }
        // macOS < 12.3 没有 ScreenCaptureKit 框架，dlopen 失败属预期，回退旧路径。
        // 句柄故意不 dlclose：框架加载后需常驻。
        let handle = unsafe {
            dlopen(
                c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit"
                    .as_ptr(),
                RTLD_LAZY,
            )
        };
        if handle.is_null() {
            log::info!("[xcap] ScreenCaptureKit framework unavailable (macOS < 12.3), SCK path disabled");
            return None;
        }
        let cls = AnyClass::get(c"SCScreenshotManager");
        if cls.is_none() {
            log::info!("[xcap] SCScreenshotManager class unavailable (macOS < 14), SCK path disabled");
        }
        cls
    }))?;

    // 类存在但方法缺失（防御未来系统变化）时同样回退。
    // 注意必须用 +respondsToSelector:（发给类对象的消息）：class_respondsToSelector
    // 只查实例方法，而 captureImageInRect:completionHandler: 是类方法。
    let has_method: bool =
        unsafe { msg_send![cls, respondsToSelector: sel!(captureImageInRect:completionHandler:)] };
    if !has_method {
        log::warn!("[xcap] SCScreenshotManager missing captureImageInRect:, SCK path disabled");
        return None;
    }

    // captureImageInRect 的 rect 是 points、全局面板坐标系，与 CGDisplayBounds 一致。
    let bounds = CGDisplayBounds(display_id);
    let call_start = Instant::now();

    // 通道元素：(结果, 转换耗时)，便于拆分 API 等待与像素转换的耗时占比。
    let (tx, rx) = std::sync::mpsc::channel::<(XCapResult<RgbaImage>, Duration)>();
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let convert_start = Instant::now();
        let result = if !error.is_null() {
            Err(XCapError::new(format!(
                "SCScreenshotManager error: {:?}",
                unsafe { &*error }
            )))
        } else if image.is_null() {
            Err(XCapError::new("SCScreenshotManager returned null image"))
        } else {
            cg_image_to_rgba(unsafe { image.as_ref() })
        };
        let _ = tx.send((result, convert_start.elapsed()));
    });

    unsafe {
        let _: () = msg_send![
            cls,
            captureImageInRect: bounds,
            completionHandler: &*block
        ];
    }

    // 超时兜底：SCK 内部异常时不至于挂死，回退旧路径。
    // 阈值 150ms：SCK 正常路径实测 25~50ms，超此即可判定异常；
    // CG 回退路径本身仅 ~76ms，500ms 等待会把最坏情况放大到 ~576ms。
    match rx.recv_timeout(Duration::from_millis(150)) {
        Ok((Ok(image), convert_elapsed)) => {
            let total = call_start.elapsed();
            log::info!(
                "[xcap] sck_capture: wait={:?} convert={:?} total={:?}",
                total - convert_elapsed,
                convert_elapsed,
                total
            );
            Some(image)
        }
        Ok((Err(err), _)) => {
            log::warn!("[xcap] SCScreenshotManager capture failed, fallback to CG: {err}");
            None
        }
        Err(err) => {
            log::warn!("[xcap] SCScreenshotManager capture timeout, fallback to CG: {err}");
            None
        }
    }
}

/// 直接读取显示器的当前帧缓冲（快速截屏路径）。
///
/// 相比 `CGWindowListCreateImage`（需枚举窗口并合成，冷启动数百毫秒），
/// `CGDisplayCreateImage` 直接抓取该显示器的当前画面（典型 20-80ms），
/// 语义即为「调用瞬间的画面」，不含鼠标光标——符合截屏选区背景的需求。
///
/// macOS 14+ 优先走 `SCScreenshotManager`（通常比 CG 路径更快），
/// 不可用/失败时自动回退 `CGDisplayCreateImage`。
pub fn capture_display(display_id: CGDirectDisplayID) -> XCapResult<RgbaImage> {
    let start = Instant::now();

    if let Some(image) = capture_display_sck(display_id) {
        log::info!("[xcap] capture_display via SCScreenshotManager: {:?}", start.elapsed());
        return Ok(image);
    }

    let cg_image = CGDisplayCreateImage(display_id)
        .ok_or_else(|| XCapError::new("CGDisplayCreateImage failed"))?;
    let result = cg_image_to_rgba(Some(cg_image.as_ref()));
    log::info!(
        "[xcap] capture_display via CGDisplayCreateImage: {:?}",
        start.elapsed()
    );
    result
}

/// 直接读取显示器指定区域的当前帧缓冲（快速截屏路径）。
/// `rect` 需为显示器全局坐标系（同 CGDisplayBounds）。
pub fn capture_display_rect(display_id: CGDirectDisplayID, rect: CGRect) -> XCapResult<RgbaImage> {
    let cg_image = CGDisplayCreateImageForRect(display_id, rect)
        .ok_or_else(|| XCapError::new("CGDisplayCreateImageForRect failed"))?;

    cg_image_to_rgba(Some(cg_image.as_ref()))
}

/// 预热 macOS 捕获后端（应用启动时调用；内部起后台线程，不阻塞调用方）。
///
/// 一次性 `SCScreenshotManager.captureImageInRect:` 首次调用需初始化捕获管线，
/// 实测首次 wait ~105ms，预热后降至 ~35ms。这里对主屏做一次抛弃式整屏截图并立即
/// 丢弃图像；macOS < 14 自动走 CG 回退（同样完成预热），无权限/失败仅记 debug 日志。
pub fn preheat_capture_backend() {
    std::thread::spawn(|| {
        let start = Instant::now();
        match capture_display(CGMainDisplayID()) {
            Ok(_) => log::info!("[xcap] macOS capture backend preheated: {:?}", start.elapsed()),
            Err(e) => log::debug!("[xcap] macOS capture backend preheat skipped: {e}"),
        }
    });
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