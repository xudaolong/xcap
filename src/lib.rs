mod error;
mod monitor;
mod video_recorder;
mod window;

#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
#[path = "linux/mod.rs"]
mod platform;

#[cfg(target_os = "android")]
#[path = "android/mod.rs"]
mod platform;

#[cfg(target_env = "ohos")]
#[path = "ohos/mod.rs"]
mod platform;

pub use image;

pub use error::{XCapError, XCapResult};
pub use monitor::Monitor;
pub use window::{Window, WindowInfo, WindowQueryOptions, WindowSizeFilter};

pub use video_recorder::Frame;
pub use video_recorder::VideoRecorder;

/// 预热 Windows DXGI 的 D3D 设备（不截屏、不缓存桌面帧）。
/// 其它平台为空操作。
pub fn preheat_capture_backend() {
    #[cfg(all(target_os = "windows", not(feature = "wgc")))]
    {
        platform::dxgi_capture::preheat_dxgi_device();
    }
}
