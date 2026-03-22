use std::env;

use xcap::{Window, WindowInfo, WindowQueryOptions, WindowSizeFilter};

fn print_window_tree(windows: &[WindowInfo], depth: usize) {
    for window in windows {
        println!(
            "{}- id={} title={:?} app={:?} pos=({}, {}, {}) size={}x{} state=({}, {}, {})",
            "  ".repeat(depth),
            window.id,
            window.title,
            window.app_name,
            window.x,
            window.y,
            window.z,
            window.width,
            window.height,
            window.is_minimized,
            window.is_maximized,
            window.is_focused
        );

        print_window_tree(&window.children, depth + 1);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = WindowQueryOptions {
        include_children: true,
        size_filter: Some(WindowSizeFilter {
            min_width: Some(300),
            max_width: None,
            min_height: Some(200),
            max_height: None,
        }),
    };

    let windows = Window::query(options.clone())?;

    print_window_tree(&windows, 0);

    #[cfg(target_os = "macos")]
    if env::var_os("XCAP_WINDOW_QUERY_DEBUG").is_some() {
        println!("\n== macOS accessibility debug ==");
        for line in Window::debug_macos_accessibility(options)? {
            println!("{line}");
        }
    }

    #[cfg(target_os = "windows")]
    if env::var_os("XCAP_WINDOW_QUERY_DEBUG").is_some() {
        println!("\n== Windows child window debug ==");
        for line in Window::debug_windows_children(options)? {
            println!("{line}");
        }
    }

    Ok(())
}
