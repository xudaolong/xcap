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

/// 预热捕获后端。
/// - Windows（DXGI）：预热 D3D 设备（不截屏、不缓存桌面帧）。
/// - macOS：后台线程做一次抛弃式 SCScreenshotManager 截图（macOS < 14 走 CG 回退），
///   预热一次性截图 API 的捕获管线（首次调用实测 ~105ms，预热后 ~35ms）。
pub fn preheat_capture_backend() {
    #[cfg(all(target_os = "windows", not(feature = "wgc")))]
    {
        platform::dxgi_capture::preheat_dxgi_device();
    }
    #[cfg(target_os = "macos")]
    {
        platform::preheat_capture_backend();
    }
}
