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
    let windows = Window::query(WindowQueryOptions {
        include_children: true,
        size_filter: Some(WindowSizeFilter {
            min_width: Some(300),
            max_width: None,
            min_height: Some(200),
            max_height: None,
        }),
    })?;

    print_window_tree(&windows, 0);

    Ok(())
}
