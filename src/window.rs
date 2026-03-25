use image::RgbaImage;
use std::collections::HashMap;

use crate::{Monitor, error::XCapResult, platform::impl_window::ImplWindow};

#[derive(Debug, Clone)]
pub struct Window {
    pub(crate) impl_window: ImplWindow,
}

impl Window {
    pub(crate) fn new(impl_window: ImplWindow) -> Window {
        Window { impl_window }
    }
}

impl Window {
    /// List all windows, sorted by z coordinate.
    pub fn all() -> XCapResult<Vec<Window>> {
        let windows = ImplWindow::all()?
            .iter()
            .map(|impl_window| Window::new(impl_window.clone()))
            .collect();

        Ok(windows)
    }

    /// Query windows with custom options.
    pub fn query(options: WindowQueryOptions) -> XCapResult<Vec<WindowInfo>> {
        ImplWindow::query(&options)
    }

    /// Query top-level windows without resolving child trees.
    pub fn query_roots(options: WindowQueryOptions) -> XCapResult<Vec<WindowInfo>> {
        ImplWindow::query_roots(&options)
    }

    /// Expand a specific window's child tree on demand.
    pub fn expand_children(
        window_id: u32,
        options: WindowQueryOptions,
    ) -> XCapResult<Vec<WindowInfo>> {
        ImplWindow::expand_children(window_id, &options)
    }

    #[cfg(target_os = "macos")]
    pub fn debug_macos_accessibility(options: WindowQueryOptions) -> XCapResult<Vec<String>> {
        ImplWindow::debug_macos_accessibility(&options)
    }

    #[cfg(target_os = "windows")]
    pub fn debug_windows_children(options: WindowQueryOptions) -> XCapResult<Vec<String>> {
        ImplWindow::debug_windows_children(&options)
    }
}

impl Window {
    /// The window id
    pub fn id(&self) -> XCapResult<u32> {
        self.impl_window.id()
    }
    /// The window process id
    pub fn pid(&self) -> XCapResult<u32> {
        self.impl_window.pid()
    }
    /// The window app name
    pub fn app_name(&self) -> XCapResult<String> {
        self.impl_window.app_name()
    }
    /// The window title
    pub fn title(&self) -> XCapResult<String> {
        self.impl_window.title()
    }
    /// The window current monitor
    pub fn current_monitor(&self) -> XCapResult<Monitor> {
        Ok(Monitor::new(self.impl_window.current_monitor()?))
    }
    /// The window x coordinate.
    pub fn x(&self) -> XCapResult<i32> {
        self.impl_window.x()
    }
    /// The window y coordinate.
    pub fn y(&self) -> XCapResult<i32> {
        self.impl_window.y()
    }
    /// The window z coordinate.
    pub fn z(&self) -> XCapResult<i32> {
        self.impl_window.z()
    }
    /// The window pixel width.
    pub fn width(&self) -> XCapResult<u32> {
        self.impl_window.width()
    }
    /// The window pixel height.
    pub fn height(&self) -> XCapResult<u32> {
        self.impl_window.height()
    }
    /// The window is minimized.
    pub fn is_minimized(&self) -> XCapResult<bool> {
        self.impl_window.is_minimized()
    }
    /// The window is maximized.
    pub fn is_maximized(&self) -> XCapResult<bool> {
        self.impl_window.is_maximized()
    }
    /// The window is focused.
    pub fn is_focused(&self) -> XCapResult<bool> {
        self.impl_window.is_focused()
    }
}

impl Window {
    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        self.impl_window.capture_image()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: u32,
    pub app_name: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub width: u32,
    pub height: u32,
    pub is_minimized: bool,
    pub is_maximized: bool,
    pub is_focused: bool,
    pub children: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowSizeFilter {
    pub min_width: Option<u32>,
    pub max_width: Option<u32>,
    pub min_height: Option<u32>,
    pub max_height: Option<u32>,
}

impl WindowSizeFilter {
    fn matches(&self, width: u32, height: u32) -> bool {
        if let Some(min_width) = self.min_width {
            if width < min_width {
                return false;
            }
        }

        if let Some(max_width) = self.max_width {
            if width > max_width {
                return false;
            }
        }

        if let Some(min_height) = self.min_height {
            if height < min_height {
                return false;
            }
        }

        if let Some(max_height) = self.max_height {
            if height > max_height {
                return false;
            }
        }

        true
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowQueryOptions {
    pub include_children: bool,
    pub size_filter: Option<WindowSizeFilter>,
    pub deep_children: bool,
    pub probe_timeout_ms: Option<u64>,
    pub relaxed_filtering: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowInfoRecord {
    pub info: WindowInfo,
    pub parent_id: Option<u32>,
}

pub(crate) fn build_window_info_tree(
    records: Vec<WindowInfoRecord>,
    options: &WindowQueryOptions,
) -> Vec<WindowInfo> {
    let mut info_by_id = HashMap::new();
    let mut child_ids_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut root_ids = Vec::new();

    for record in &records {
        info_by_id.insert(record.info.id, record.info.clone());
    }

    for record in &records {
        match record.parent_id {
            Some(parent_id) if info_by_id.contains_key(&parent_id) => {
                child_ids_by_parent
                    .entry(parent_id)
                    .or_default()
                    .push(record.info.id);
            }
            _ => root_ids.push(record.info.id),
        }
    }

    fn build_node(
        id: u32,
        info_by_id: &HashMap<u32, WindowInfo>,
        child_ids_by_parent: &HashMap<u32, Vec<u32>>,
        options: &WindowQueryOptions,
        visiting: &mut std::collections::HashSet<u32>,
    ) -> Vec<WindowInfo> {
        if !visiting.insert(id) {
            return Vec::new();
        }

        let Some(mut node) = info_by_id.get(&id).cloned() else {
            visiting.remove(&id);
            return Vec::new();
        };

        let promoted_children = child_ids_by_parent
            .get(&id)
            .into_iter()
            .flatten()
            .flat_map(|child_id| {
                build_node(
                    *child_id,
                    info_by_id,
                    child_ids_by_parent,
                    options,
                    visiting,
                )
            })
            .collect::<Vec<_>>();

        if options.include_children {
            node.children = promoted_children.clone();
        } else {
            node.children.clear();
        }

        let matches = options
            .size_filter
            .as_ref()
            .is_none_or(|filter| filter.matches(node.width, node.height));

        let result = if matches {
            vec![node]
        } else if options.include_children {
            promoted_children
        } else {
            Vec::new()
        };

        visiting.remove(&id);
        result
    }

    root_ids
        .into_iter()
        .flat_map(|id| {
            let mut visiting = std::collections::HashSet::new();
            build_node(
                id,
                &info_by_id,
                &child_ids_by_parent,
                options,
                &mut visiting,
            )
        })
        .collect()
}

pub(crate) fn build_expanded_children(
    root: WindowInfo,
    descendants: Vec<WindowInfoRecord>,
    options: &WindowQueryOptions,
) -> Vec<WindowInfo> {
    let root_id = root.id;
    let mut records = Vec::with_capacity(descendants.len() + 1);
    records.push(WindowInfoRecord {
        info: root,
        parent_id: None,
    });
    records.extend(descendants);

    let mut tree_options = options.clone();
    tree_options.include_children = true;

    let mut children = build_window_info_tree(records, &tree_options)
        .into_iter()
        .find(|node| node.id == root_id)
        .map(|node| node.children)
        .unwrap_or_default();

    if !options.include_children {
        clear_nested_children(&mut children);
    }

    children
}

fn clear_nested_children(nodes: &mut [WindowInfo]) {
    for node in nodes {
        node.children.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_info(id: u32, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            id,
            pid: id,
            app_name: format!("app-{id}"),
            title: format!("window-{id}"),
            x: 0,
            y: 0,
            z: id as i32,
            width,
            height,
            is_minimized: false,
            is_maximized: false,
            is_focused: false,
            children: Vec::new(),
        }
    }

    #[test]
    fn test_build_window_info_tree_with_children() {
        let records = vec![
            WindowInfoRecord {
                info: window_info(1, 800, 600),
                parent_id: None,
            },
            WindowInfoRecord {
                info: window_info(2, 400, 300),
                parent_id: Some(1),
            },
            WindowInfoRecord {
                info: window_info(3, 200, 100),
                parent_id: Some(2),
            },
        ];

        let result = build_window_info_tree(
            records,
            &WindowQueryOptions {
                include_children: true,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 1);
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[0].children[0].id, 2);
        assert_eq!(result[0].children[0].children.len(), 1);
        assert_eq!(result[0].children[0].children[0].id, 3);
    }

    #[test]
    fn test_build_window_info_tree_without_children() {
        let records = vec![
            WindowInfoRecord {
                info: window_info(1, 800, 600),
                parent_id: None,
            },
            WindowInfoRecord {
                info: window_info(2, 400, 300),
                parent_id: Some(1),
            },
        ];

        let result = build_window_info_tree(
            records,
            &WindowQueryOptions {
                include_children: false,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );

        assert_eq!(result.len(), 1);
        assert!(result[0].children.is_empty());
    }

    #[test]
    fn test_build_window_info_tree_filters_by_size() {
        let records = vec![
            WindowInfoRecord {
                info: window_info(1, 300, 200),
                parent_id: None,
            },
            WindowInfoRecord {
                info: window_info(2, 800, 600),
                parent_id: None,
            },
        ];

        let result = build_window_info_tree(
            records,
            &WindowQueryOptions {
                include_children: false,
                size_filter: Some(WindowSizeFilter {
                    min_width: Some(500),
                    max_width: None,
                    min_height: Some(400),
                    max_height: None,
                }),
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 2);
    }

    #[test]
    fn test_filtered_parent_promotes_matching_children() {
        let records = vec![
            WindowInfoRecord {
                info: window_info(1, 200, 100),
                parent_id: None,
            },
            WindowInfoRecord {
                info: window_info(2, 600, 400),
                parent_id: Some(1),
            },
            WindowInfoRecord {
                info: window_info(3, 700, 500),
                parent_id: Some(2),
            },
        ];

        let result = build_window_info_tree(
            records,
            &WindowQueryOptions {
                include_children: true,
                size_filter: Some(WindowSizeFilter {
                    min_width: Some(500),
                    max_width: None,
                    min_height: Some(300),
                    max_height: None,
                }),
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 2);
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[0].children[0].id, 3);
    }

    #[test]
    fn test_build_window_info_tree_skips_cycles() {
        let records = vec![
            WindowInfoRecord {
                info: window_info(1, 600, 400),
                parent_id: Some(2),
            },
            WindowInfoRecord {
                info: window_info(2, 500, 300),
                parent_id: Some(1),
            },
            WindowInfoRecord {
                info: window_info(3, 700, 500),
                parent_id: None,
            },
        ];

        let result = build_window_info_tree(
            records,
            &WindowQueryOptions {
                include_children: true,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 3);
    }

    #[test]
    fn test_window_query_options_default_is_conservative() {
        assert_eq!(
            WindowQueryOptions::default(),
            WindowQueryOptions {
                include_children: false,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            }
        );
    }

    #[test]
    fn test_build_expanded_children_respects_include_children() {
        let root = window_info(1, 800, 600);
        let descendants = vec![
            WindowInfoRecord {
                info: window_info(2, 400, 300),
                parent_id: Some(1),
            },
            WindowInfoRecord {
                info: window_info(3, 200, 100),
                parent_id: Some(2),
            },
        ];

        let flat_children = build_expanded_children(
            root.clone(),
            descendants.clone(),
            &WindowQueryOptions {
                include_children: false,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );
        assert_eq!(flat_children.len(), 1);
        assert!(flat_children[0].children.is_empty());

        let tree_children = build_expanded_children(
            root,
            descendants,
            &WindowQueryOptions {
                include_children: true,
                size_filter: None,
                deep_children: false,
                probe_timeout_ms: None,
                relaxed_filtering: false,
            },
        );
        assert_eq!(tree_children.len(), 1);
        assert_eq!(tree_children[0].children.len(), 1);
    }
}
