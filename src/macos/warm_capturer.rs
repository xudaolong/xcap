//! 常驻「预热捕获」流（macOS-only）。
//!
//! `CGWindowListCreateImage` 每次调用冷启动成本高达数百毫秒（截屏体感的
//! 主要瓶颈）。本模块用 `AVCaptureScreenInput` 常驻一条低帧率捕获流，
//! 持续把最新帧写进内存缓存；`capture_image` 优先读缓存（~1ms），
//! CG 路径仅作为冷启动/异常时的 fallback。
//!
//! 生命周期策略（由上层决定）：
//! - 应用预热阶段启动流（`warm_start`），闲时 10 fps，低功耗；
//! - 截图会话期间切到 30 fps 并主动取一帧，保证「按下即所见」；
//! - 长时间无截图时 `warm_stop` 释放 Session（ScreenCaptureKit 会消耗资源）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::RgbaImage;
use objc2::{
    AllocAnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_av_foundation::{
    AVCaptureConnection, AVCaptureOutput, AVCaptureScreenInput, AVCaptureSession,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
};
use objc2_core_graphics::CGDirectDisplayID;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetDataSize,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32ARGB, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_32RGBA,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};
use scopeguard::defer;

use crate::{XCapError, XCapResult};

/// 缓存中最新一帧的像素数据（已统一为 RGBA）
#[derive(Debug, Default)]
struct LatestFrame {
    width: u32,
    height: u32,
    raw: Vec<u8>,
    /// 帧采集时间（delegate 写入时记录），供新鲜度校验；无帧时为 None
    captured_at: Option<Instant>,
}

/// Delegate 内部状态：最新帧 + 运行标志 + 帧序号
#[derive(Debug)]
struct WarmCapturerDelegateVars {
    latest: Arc<Mutex<LatestFrame>>,
    running: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
}

fn is_supported_format(format_type: u32) -> bool {
    matches!(
        format_type,
        kCVPixelFormatType_32ARGB | kCVPixelFormatType_32BGRA | kCVPixelFormatType_32RGBA
    )
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "WarmCapturerSampleBufferDelegate"]
    #[ivars = WarmCapturerDelegateVars]
    #[derive(Debug)]
    struct WarmCapturerSampleBufferDelegate;

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for WarmCapturerSampleBufferDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn capture_output_did_output_sample_buffer_from_connection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if !self.ivars().running.load(Ordering::Acquire) {
                return;
            }

            let pixel_buffer = match CMSampleBuffer::image_buffer(sample_buffer) {
                Some(pixel_buffer) => pixel_buffer,
                None => return,
            };

            CVPixelBufferLockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
            defer! {
                CVPixelBufferUnlockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
            };

            let format_type = CVPixelBufferGetPixelFormatType(&pixel_buffer);
            if !is_supported_format(format_type) {
                return;
            }

            let width = CVPixelBufferGetWidth(&pixel_buffer);
            let height = CVPixelBufferGetHeight(&pixel_buffer);
            let bytes_per_row = CVPixelBufferGetBytesPerRow(&pixel_buffer);
            let base_address = CVPixelBufferGetBaseAddress(&pixel_buffer);
            let size = CVPixelBufferGetDataSize(&pixel_buffer);

            if base_address.is_null() || size == 0 {
                return;
            }

            let row_len = width * 4;
            let data = std::slice::from_raw_parts(base_address.cast::<u8>(), size);

            // 去行尾 padding
            let mut raw = if bytes_per_row == row_len {
                data[..row_len * height].to_vec()
            } else {
                let mut buf = vec![0u8; row_len * height];
                for (src, dst) in data
                    .chunks_exact(bytes_per_row)
                    .zip(buf.chunks_exact_mut(row_len))
                {
                    dst.copy_from_slice(&src[..row_len]);
                }
                buf
            };
            // 统一为 RGBA
            match format_type {
                kCVPixelFormatType_32BGRA => bgra_to_rgba_inplace(&mut raw),
                kCVPixelFormatType_32ARGB => argb_to_rgba_inplace(&mut raw),
                _ => {}
            }

            let mut latest = match self.ivars().latest.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            latest.width = width as u32;
            latest.height = height as u32;
            latest.raw = raw;
            latest.captured_at = Some(Instant::now());
            self.ivars().seq.fetch_add(1, Ordering::Relaxed);
        }
    }
);

unsafe impl NSObjectProtocol for WarmCapturerSampleBufferDelegate {}

impl WarmCapturerSampleBufferDelegate {
    fn new(
        latest: Arc<Mutex<LatestFrame>>,
        running: Arc<AtomicBool>,
        seq: Arc<AtomicU64>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(WarmCapturerDelegateVars {
            latest,
            running,
            seq,
        });
        unsafe { msg_send![super(this), init] }
    }
}

fn bgra_to_rgba_inplace(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

fn argb_to_rgba_inplace(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
        px[0] = r;
        px[1] = g;
        px[2] = b;
        px[3] = a;
    }
}

/// 常驻预热捕获器（macOS-only）。
///
/// 同一时间只维护一块显示器的流；切换显示器先 stop 再 start。
pub struct WarmCapturer {
    session: Retained<AVCaptureSession>,
    _input: Retained<AVCaptureScreenInput>,
    _output: Retained<AVCaptureVideoDataOutput>,
    _delegate: Retained<WarmCapturerSampleBufferDelegate>,
    running: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
    latest: Arc<Mutex<LatestFrame>>,
}

// AVCaptureSession / AVCaptureScreenInput 线程安全（Apple 文档），
// dispatch2::DispatchQueue 本身不 Send，但我们从不跨线程移走它。
unsafe impl Send for WarmCapturer {}
unsafe impl Sync for WarmCapturer {}

impl WarmCapturer {
    pub fn new(cg_direct_display_id: CGDirectDisplayID) -> XCapResult<Self> {
        unsafe {
            let session = AVCaptureSession::new();
            let input = AVCaptureScreenInput::initWithDisplayID(
                AVCaptureScreenInput::alloc(),
                cg_direct_display_id,
            )
            .ok_or_else(|| XCapError::new("AVCaptureScreenInput::initWithDisplayID failed"))?;
            input.setCapturesCursor(true);
            // 30fps：足够新鲜（每帧 ~33ms），省电与新鲜度折中
            let min_frame_duration = CMTime::new(1, 30);
            let _: () = msg_send![&input, setMinFrameDuration: min_frame_duration];

            if session.canAddInput(&input) {
                session.addInput(&input);
            }

            let output = AVCaptureVideoDataOutput::new();
            output.setAlwaysDiscardsLateVideoFrames(true);
            output.setAutomaticallyConfiguresOutputBufferDimensions(true);

            let format_type_key =
                NSString::from_str(kCVPixelBufferPixelFormatTypeKey.to_string().as_str());
            let available = output.availableVideoCVPixelFormatTypes();
            // 优先 RGBA（零转换），其次 BGRA/ARGB（一次行内转换）
            let preferred = [
                kCVPixelFormatType_32RGBA,
                kCVPixelFormatType_32BGRA,
                kCVPixelFormatType_32ARGB,
            ];
            let format_type = preferred
                .into_iter()
                .find(|f| available.containsObject(&NSNumber::new_u32(*f)))
                .ok_or_else(|| XCapError::new("no supported 32-bit pixel format in warm capturer"))?;

            let video_settings = NSDictionary::from_slices::<NSString>(
                &[format_type_key.as_ref()],
                &[NSNumber::new_u32(format_type).as_ref()],
            );
            output.setVideoSettings(Some(&video_settings));

            if session.canAddOutput(&output) {
                session.addOutput(&output);
            }

            let running = Arc::new(AtomicBool::new(false));
            let seq = Arc::new(AtomicU64::new(0));
            let latest: Arc<Mutex<LatestFrame>> = Arc::new(Mutex::new(LatestFrame::default()));
            let delegate = WarmCapturerSampleBufferDelegate::new(
                latest.clone(),
                running.clone(),
                seq.clone(),
            );

            let sample_buffer_delegate = ProtocolObject::<
                dyn AVCaptureVideoDataOutputSampleBufferDelegate,
            >::from_ref(&*delegate);

            let queue = dispatch2::DispatchQueue::new(
                "WarmCapturerSampleBufferDelegate",
                dispatch2::DispatchQueueAttr::SERIAL,
            );
            let queue: &dispatch2::DispatchQueue = queue.as_ref();
            let _: () = msg_send![&output, setSampleBufferDelegate: sample_buffer_delegate, queue: queue];

            Ok(Self {
                session,
                _input: input,
                _output: output,
                _delegate: delegate,
                running,
                seq,
                latest,
            })
        }
    }

    pub fn start(&self) -> XCapResult<()> {
        self.running.store(true, Ordering::Release);
        unsafe { self.session.startRunning() };
        Ok(())
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.running.store(false, Ordering::Release);
        unsafe { self.session.stopRunning() };
        Ok(())
    }

    /// 读取最新一帧（RGBA）。无帧或流未运行返回 None。
    pub fn latest_frame(&self) -> Option<RgbaImage> {
        let latest = self.latest.lock().ok()?;
        if latest.raw.is_empty() {
            return None;
        }
        RgbaImage::from_raw(latest.width, latest.height, latest.raw.clone())
    }

    /// 当前帧序号（用于判断缓存是否刷新）
    pub fn frame_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// 等待一帧「比进入时更新」的帧（AVCaptureScreenInput 按最小帧间隔持续出帧）。
    /// 最多等 `timeout`，期间流停止或无新帧则尽早返回当前缓存。
    /// 返回 `(是否拿到新帧, 最新帧)`；最新帧可能为空（流尚未出过帧）。
    pub fn wait_fresh_frame(&self, timeout: Duration) -> (bool, Option<RgbaImage>) {
        let seq0 = self.frame_seq();
        let deadline = Instant::now() + timeout;
        loop {
            if self.frame_seq() > seq0 {
                let frame = self.latest_frame();
                return (true, frame);
            }
            if !self.running.load(Ordering::Acquire) {
                // 流已停止，不再等新帧
                return (false, self.latest_frame());
            }
            if Instant::now() >= deadline {
                return (false, self.latest_frame());
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    /// 最新帧的采集时间（用于 age 校验）
    pub fn latest_frame_age(&self) -> Option<Duration> {
        let latest = self.latest.lock().ok()?;
        latest.captured_at.map(|t| t.elapsed())
    }
}