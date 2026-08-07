use std::sync::mpsc::Receiver;

use image::RgbaImage;

use crate::{
    VideoRecorder, error::XCapResult, platform::impl_monitor::ImplMonitor, video_recorder::Frame,
};

#[derive(Debug, Clone)]
pub struct Monitor {
    pub(crate) impl_monitor: ImplMonitor,
}

impl Monitor {
    pub(crate) fn new(impl_monitor: ImplMonitor) -> Monitor {
        Monitor { impl_monitor }
    }
}

impl Monitor {
    pub fn all() -> XCapResult<Vec<Monitor>> {
        let monitors = ImplMonitor::all()?
            .iter()
            .map(|impl_monitor| Monitor::new(impl_monitor.clone()))
            .collect();

        Ok(monitors)
    }

    pub fn from_point(x: i32, y: i32) -> XCapResult<Monitor> {
        let impl_monitor = ImplMonitor::from_point(x, y)?;

        Ok(Monitor::new(impl_monitor))
    }
}

impl Monitor {
    /// Unique identifier associated with the screen.
    pub fn id(&self) -> XCapResult<u32> {
        self.impl_monitor.id()
    }
    /// The display name
    pub fn name(&self) -> XCapResult<String> {
        self.impl_monitor.name()
    }
    /// The display friendly name
    pub fn friendly_name(&self) -> XCapResult<String> {
        self.impl_monitor.friendly_name()
    }
    /// The screen x coordinate.
    pub fn x(&self) -> XCapResult<i32> {
        self.impl_monitor.x()
    }
    /// The screen x coordinate.
    pub fn y(&self) -> XCapResult<i32> {
        self.impl_monitor.y()
    }
    /// The screen pixel width.
    pub fn width(&self) -> XCapResult<u32> {
        self.impl_monitor.width()
    }
    /// The screen pixel height.
    pub fn height(&self) -> XCapResult<u32> {
        self.impl_monitor.height()
    }
    /// Can be 0, 90, 180, 270, represents screen rotation in clock-wise degrees.
    pub fn rotation(&self) -> XCapResult<f32> {
        self.impl_monitor.rotation()
    }
    /// Output device's pixel scale factor.
    pub fn scale_factor(&self) -> XCapResult<f32> {
        self.impl_monitor.scale_factor()
    }
    /// The screen refresh rate.
    pub fn frequency(&self) -> XCapResult<f32> {
        self.impl_monitor.frequency()
    }
    /// Whether the screen is the main screen
    pub fn is_primary(&self) -> XCapResult<bool> {
        self.impl_monitor.is_primary()
    }

    /// Whether the screen is builtin
    pub fn is_builtin(&self) -> XCapResult<bool> {
        self.impl_monitor.is_builtin()
    }
}

impl Monitor {
    /// Capture image of the monitor
    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_image()
    }

    pub fn capture_region(&self, x: u32, y: u32, width: u32, height: u32) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_region(x, y, width, height)
    }

    pub fn video_recorder(&self) -> XCapResult<(VideoRecorder, Receiver<Frame>)> {
        let (impl_video_recorder, sx) = self.impl_monitor.video_recorder()?;

        Ok((VideoRecorder::new(impl_video_recorder), sx))
    }
}

// ============ 预热捕获流（Warm Capturer）============

impl Monitor {
    /// 启动（或切换到）常驻预热捕获流，持续把最新帧写入内存缓存。
    /// `capture_image` 将优先读取该缓存（毫秒级），大幅降低截屏冷启动延迟。
    ///
    /// 同一时间只保留一块显示器的流；重复调用相同显示器时直接复用。
    /// 返回 true 表示新建了流，false 表示复用已有流。
    pub fn warm_capture_start(&self) -> XCapResult<bool> {
        self.impl_monitor.warm_capture_start()
    }

    /// 停止并释放预热捕获流（长时间无截图时可调用以释放资源）。
    pub fn warm_capture_stop(&self) -> XCapResult<()> {
        self.impl_monitor.warm_capture_stop()
    }

    /// 读取预热流的最新一帧（未启动或尚无帧时返回 None）。
    pub fn warm_capture_latest_frame(&self) -> XCapResult<Option<RgbaImage>> {
        self.impl_monitor.warm_capture_latest_frame()
    }
}

#[cfg(test)]
mod tests {
    use crate::XCapError;

    use super::*;

    #[test]
    fn test_capture_region_out_of_bounds() {
        let monitors = Monitor::all().unwrap();
        let Some(monitor) = monitors.first() else {
            return;
        };

        // Try to capture a region that extends beyond monitor bounds
        let x = monitor.width().unwrap() / 2;
        let y = monitor.height().unwrap() / 2;
        let width = monitor.width().unwrap();
        let height = monitor.height().unwrap();

        let result = monitor.capture_region(x, y, width, height);

        match result {
            Err(XCapError::InvalidCaptureRegion(_)) => (),
            _ => panic!("Expected InvalidCaptureRegion error"),
        }
    }
}
