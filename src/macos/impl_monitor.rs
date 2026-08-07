use std::sync::mpsc::Receiver;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use image::RgbaImage;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive,
    CGDisplayIsBuiltin, CGDisplayIsMain, CGDisplayMode, CGDisplayModelNumber, CGDisplayRotation,
    CGError, CGGetActiveDisplayList, CGGetDisplaysWithPoint, CGWindowImageOption,
    CGWindowListOption,
};
use objc2_foundation::{NSNumber, NSString};

use crate::{
    error::{XCapError, XCapResult},
    video_recorder::Frame,
};

use super::{capture::capture, impl_video_recorder::ImplVideoRecorder, warm_capturer::WarmCapturer};

#[derive(Debug, Clone)]
pub(crate) struct ImplMonitor {
    pub cg_direct_display_id: CGDirectDisplayID,
}

/// 常驻预热捕获流（同一时间只跟踪一块显示器）。
/// 由 CodeExpander 启动时/截图会话开始时调用 warm_capture_start 建立；
/// `capture_image` 优先读 warm 流的最新帧，避免每次 CGWindowListCreateImage 的冷启动开销。
static WARM_CAPTURER: LazyLock<Mutex<Option<WarmCapturer>>> =
    LazyLock::new(|| Mutex::new(None));
static WARM_DISPLAY_ID: Mutex<Option<CGDirectDisplayID>> = Mutex::new(None);

/// 启动/切换预热捕获流。同一时间只保留一块显示器。
/// 返回 false 表示该显示器已有流（无需重建）。
pub fn warm_capture_start(display_id: CGDirectDisplayID) -> XCapResult<bool> {
    let current = *WARM_DISPLAY_ID.lock()?;
    if current == Some(display_id) {
        if let Some(capturer) = WARM_CAPTURER.lock()?.as_ref() {
            let _ = capturer.start();
            return Ok(false);
        }
    }

    let capturer = WarmCapturer::new(display_id)?;
    capturer.start()?;
    let mut guard = WARM_CAPTURER.lock()?;
    *guard = Some(capturer);
    *WARM_DISPLAY_ID.lock()? = Some(display_id);
    Ok(true)
}

/// 停止预热捕获流（例如长时间未截屏时释放资源）
pub fn warm_capture_stop() -> XCapResult<()> {
    let mut guard = WARM_CAPTURER.lock()?;
    if let Some(capturer) = guard.as_ref() {
        let _ = capturer.stop();
    }
    *guard = None;
    Ok(())
}

/// 读取预热流的最新一帧（若有）
pub fn warm_capture_latest_frame() -> XCapResult<Option<RgbaImage>> {
    let guard = WARM_CAPTURER.lock()?;
    Ok(guard.as_ref().and_then(|c| c.latest_frame()))
}

/// 预热流帧允许的最大年龄：超过则视为 stale，走 CG 同步截屏保底准确
const WARM_FRAME_MAX_AGE: Duration = Duration::from_millis(150);

impl ImplMonitor {
    pub fn warm_capture_start(&self) -> XCapResult<bool> {
        warm_capture_start(self.cg_direct_display_id)
    }

    pub fn warm_capture_stop(&self) -> XCapResult<()> {
        warm_capture_stop()
    }

    pub fn warm_capture_latest_frame(&self) -> XCapResult<Option<RgbaImage>> {
        warm_capture_latest_frame()
    }
}

fn get_display_friendly_name(display_id: CGDirectDisplayID) -> XCapResult<String> {
    let screens = NSScreen::screens(unsafe { MainThreadMarker::new_unchecked() });
    for screen in screens {
        let device_description = screen.deviceDescription();
        let screen_number = device_description
            .objectForKey(&NSString::from_str("NSScreenNumber"))
            .ok_or(XCapError::new("Get NSScreenNumber failed"))?;

        let screen_id = screen_number
            .downcast::<NSNumber>()
            .map_err(|err| XCapError::new(format!("{:?}", err)))?
            .unsignedIntValue();

        if screen_id == display_id {
            return Ok(screen.localizedName().to_string());
        }
    }

    Err(XCapError::new(format!(
        "Get display {} friendly name failed",
        display_id
    )))
}

impl ImplMonitor {
    pub fn new(cg_direct_display_id: CGDirectDisplayID) -> ImplMonitor {
        ImplMonitor {
            cg_direct_display_id,
        }
    }
    pub fn all() -> XCapResult<Vec<ImplMonitor>> {
        let max_displays: u32 = 16;
        let mut active_displays: Vec<CGDirectDisplayID> = vec![0; max_displays as usize];
        let mut display_count: u32 = 0;

        let cg_error = unsafe {
            CGGetActiveDisplayList(
                max_displays,
                active_displays.as_mut_ptr(),
                &mut display_count,
            )
        };

        if cg_error != CGError::Success {
            return Err(XCapError::new(format!(
                "CGGetActiveDisplayList failed: {:?}",
                cg_error
            )));
        }

        active_displays.truncate(display_count as usize);

        let mut impl_monitors = Vec::with_capacity(active_displays.len());

        for display in active_displays {
            impl_monitors.push(ImplMonitor::new(display));
        }

        Ok(impl_monitors)
    }

    pub fn from_point(x: i32, y: i32) -> XCapResult<ImplMonitor> {
        let point = CGPoint {
            x: x as f64,
            y: y as f64,
        };

        let max_displays: u32 = 16;
        let mut display_ids: Vec<CGDirectDisplayID> = vec![0; max_displays as usize];
        let mut display_count: u32 = 0;

        let cg_error = unsafe {
            CGGetDisplaysWithPoint(
                point,
                max_displays,
                display_ids.as_mut_ptr(),
                &mut display_count,
            )
        };

        if cg_error != CGError::Success {
            return Err(XCapError::new(format!(
                "CGGetDisplaysWithPoint failed: {:?}",
                cg_error
            )));
        }

        if display_count == 0 {
            return Err(XCapError::new("Monitor not found"));
        }

        if let Some(&display_id) = display_ids.first() {
            if !CGDisplayIsActive(display_id) {
                return Err(XCapError::new("Monitor is not active"));
            }
            Ok(ImplMonitor::new(display_id))
        } else {
            Err(XCapError::new("Monitor not found"))
        }
    }
}

impl ImplMonitor {
    pub fn id(&self) -> XCapResult<u32> {
        Ok(self.cg_direct_display_id)
    }

    pub fn name(&self) -> XCapResult<String> {
        let screen_number = CGDisplayModelNumber(self.cg_direct_display_id);
        Ok(format!("Display #{}", screen_number))
    }

    pub fn friendly_name(&self) -> XCapResult<String> {
        let name = get_display_friendly_name(self.cg_direct_display_id)
            .map_err(|_| self.name())
            .unwrap_or(format!("Unknown Monitor {}", self.cg_direct_display_id));

        Ok(name)
    }

    pub fn x(&self) -> XCapResult<i32> {
        let rect = CGDisplayBounds(self.cg_direct_display_id);

        Ok(rect.origin.x as i32)
    }

    pub fn y(&self) -> XCapResult<i32> {
        let cg_rect = CGDisplayBounds(self.cg_direct_display_id);

        Ok(cg_rect.origin.y as i32)
    }

    pub fn width(&self) -> XCapResult<u32> {
        let cg_rect = CGDisplayBounds(self.cg_direct_display_id);

        Ok(cg_rect.size.width as u32)
    }

    pub fn height(&self) -> XCapResult<u32> {
        let cg_rect = CGDisplayBounds(self.cg_direct_display_id);

        Ok(cg_rect.size.height as u32)
    }

    pub fn rotation(&self) -> XCapResult<f32> {
        let rotation = CGDisplayRotation(self.cg_direct_display_id);

        Ok(rotation as f32)
    }

    pub fn scale_factor(&self) -> XCapResult<f32> {
        let display_mode = CGDisplayCopyDisplayMode(self.cg_direct_display_id);
        let pixel_width = CGDisplayMode::pixel_width(display_mode.as_deref());
        let width = self.width()?;

        Ok(pixel_width as f32 / width as f32)
    }

    pub fn frequency(&self) -> XCapResult<f32> {
        let frequency = {
            let display_mode = CGDisplayCopyDisplayMode(self.cg_direct_display_id);
            CGDisplayMode::refresh_rate(display_mode.as_deref())
        };

        Ok(frequency as f32)
    }

    pub fn is_primary(&self) -> XCapResult<bool> {
        let is_primary = CGDisplayIsMain(self.cg_direct_display_id);

        Ok(is_primary)
    }

    pub fn is_builtin(&self) -> XCapResult<bool> {
        let is_builtin = CGDisplayIsBuiltin(self.cg_direct_display_id);

        Ok(is_builtin)
    }

    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        // 优先用预热流：等一帧「新帧」再取，避免返回陈旧画面；
        // 等待超时或帧过旧时 fallback 到 CG 同步截屏（保证准确）。
        let warm_display = *WARM_DISPLAY_ID.lock()?;
        if warm_display == Some(self.cg_direct_display_id) {
            if let Some(capturer) = WARM_CAPTURER.lock()?.as_ref() {
                let (fresh, image) = capturer.wait_fresh_frame(Duration::from_millis(100));
                if let Some(image) = image {
                    let age_ok = capturer
                        .latest_frame_age()
                        .map(|age| age <= WARM_FRAME_MAX_AGE)
                        .unwrap_or(false);
                    if fresh && age_ok {
                        return Ok(image);
                    }
                    if !fresh && age_ok {
                        // 等待超时（流未出新帧），但缓存帧仍新鲜，可用
                        return Ok(image);
                    }
                }
            }
        }

        let cg_rect = CGDisplayBounds(self.cg_direct_display_id);

        capture(
            cg_rect,
            CGWindowListOption::OptionOnScreenOnly,
            0,
            CGWindowImageOption::ShouldBeOpaque,
        )
    }

    pub fn capture_region(&self, x: u32, y: u32, width: u32, height: u32) -> XCapResult<RgbaImage> {
        // Validate region bounds
        let monitor_x = self.x()?;
        let monitor_y = self.y()?;
        let monitor_width = self.width()?;
        let monitor_height = self.height()?;

        if width > monitor_width
            || height > monitor_height
            || x + width > monitor_width
            || y + height > monitor_height
        {
            return Err(XCapError::InvalidCaptureRegion(format!(
                "Region ({}, {}, {}, {}) is outside monitor bounds ({}, {}, {}, {})",
                x, y, width, height, monitor_x, monitor_y, monitor_width, monitor_height
            )));
        }

        // Create a CGRect for the region to capture
        let cg_rect = objc2_core_foundation::CGRect {
            origin: objc2_core_foundation::CGPoint {
                x: (monitor_x + x as i32) as f64,
                y: (monitor_y + y as i32) as f64,
            },
            size: objc2_core_foundation::CGSize {
                width: width as f64,
                height: height as f64,
            },
        };

        capture(
            cg_rect,
            CGWindowListOption::OptionOnScreenOnly,
            0,
            CGWindowImageOption::ShouldBeOpaque,
        )
    }

    pub fn video_recorder(&self) -> XCapResult<(ImplVideoRecorder, Receiver<Frame>)> {
        ImplVideoRecorder::new(self.cg_direct_display_id)
    }
}
