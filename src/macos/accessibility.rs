use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    ptr::{self, NonNull},
};

use objc2_core_foundation::{
    CFArray, CFBoolean, CFRetained, CFString, CFType, CFTypeID, CGPoint, CGSize, ConcreteType, Type,
};

use crate::window::{WindowInfo, WindowInfoRecord};

const AX_ERROR_SUCCESS: i32 = 0;
const AX_VALUE_TYPE_CGPOINT: u32 = 1;
const AX_VALUE_TYPE_CGSIZE: u32 = 2;
const BASE_MAX_AX_DEPTH: usize = 6;
const DEEP_MAX_AX_DEPTH: usize = 9;
const MAX_AX_CHILDREN_PER_NODE: usize = 128;
const MAX_AX_NODES_PER_WINDOW: usize = 1024;
const BASE_CHILD_ATTRIBUTES: &[&str] = &["AXChildren"];
const DEEP_CHILD_ATTRIBUTES: &[&str] = &[
    "AXChildren",
    "AXContents",
    "AXVisibleChildren",
    "AXRows",
    "AXColumns",
    "AXTabs",
    "AXToolbar",
    "AXSheets",
    "AXDrawer",
];

#[derive(Debug)]
#[repr(C)]
pub(super) struct AXUIElement {
    _inner: CFType,
}

impl AsRef<CFType> for AXUIElement {
    fn as_ref(&self) -> &CFType {
        &self._inner
    }
}

unsafe impl Type for AXUIElement {}

unsafe impl ConcreteType for AXUIElement {
    fn type_id() -> CFTypeID {
        unsafe { AXUIElementGetTypeID() }
    }
}

#[derive(Debug)]
#[repr(C)]
struct AXValue {
    _inner: CFType,
}

impl AsRef<CFType> for AXValue {
    fn as_ref(&self) -> &CFType {
        &self._inner
    }
}

unsafe impl Type for AXValue {}

unsafe impl ConcreteType for AXValue {
    fn type_id() -> CFTypeID {
        unsafe { AXValueGetTypeID() }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C-unwind" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXUIElementCreateApplication(pid: i32) -> *mut AXUIElement;
    fn AXUIElementCopyAttributeValue(
        element: &AXUIElement,
        attribute: &CFString,
        value: *mut *const c_void,
    ) -> i32;
    fn AXValueGetType(value: &AXValue) -> u32;
    fn AXValueGetValue(value: &AXValue, the_type: u32, value_ptr: *mut c_void) -> u8;
}

unsafe extern "C-unwind" {
    fn CFArrayGetCount(the_array: &CFArray) -> isize;
    fn CFArrayGetValueAtIndex(the_array: &CFArray, idx: isize) -> *const c_void;
}

#[derive(Debug)]
struct AccessibilityWindow {
    element: CFRetained<AXUIElement>,
    title: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
pub(super) struct AccessibilityApp {
    windows: Vec<AccessibilityWindow>,
    focused_element_ptr: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AccessibilityQueryOptions {
    pub deep_children: bool,
    pub relaxed_filtering: bool,
}

pub(super) fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

pub(super) fn application(pid: u32) -> Option<AccessibilityApp> {
    let ptr = NonNull::new(unsafe { AXUIElementCreateApplication(pid as i32) })?;
    let application = unsafe { CFRetained::from_raw(ptr) };

    let focused_element_ptr = ax_copy_attribute_value(&application, "AXFocusedUIElement")
        .and_then(|value| value.downcast::<AXUIElement>().ok())
        .map(|element| element_ptr_key(&element));

    let windows = ax_array_attribute_elements(&application, "AXWindows")
        .into_iter()
        .filter_map(|element| {
            let (x, y) = ax_position_attribute(&element).unwrap_or_default();
            let (width, height) = ax_size_attribute(&element).unwrap_or_default();

            Some(AccessibilityWindow {
                title: ax_string_attribute(&element, "AXTitle").unwrap_or_default(),
                x,
                y,
                width,
                height,
                element,
            })
        })
        .collect::<Vec<_>>();

    Some(AccessibilityApp {
        windows,
        focused_element_ptr,
    })
}

pub(super) fn descendant_records_for_window(
    root: &WindowInfo,
    app: &AccessibilityApp,
    options: AccessibilityQueryOptions,
    next_id: &mut u32,
) -> Vec<WindowInfoRecord> {
    let Some(window) = match_window(root, app) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    let mut visited = HashSet::new();
    visited.insert(element_ptr_key(&window.element));
    let mut visited_count = 1usize;
    collect_descendant_records(
        root.id,
        &window.element,
        root.pid,
        &root.app_name,
        app.focused_element_ptr,
        options,
        0,
        &mut visited,
        &mut visited_count,
        next_id,
        &mut records,
    );

    records
}

pub(super) fn debug_lines_for_window(
    root: &WindowInfo,
    app: Option<&AccessibilityApp>,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "CG window id={} pid={} title={:?} app={:?} pos=({}, {}) size={}x{}",
        root.id, root.pid, root.title, root.app_name, root.x, root.y, root.width, root.height
    ));

    let Some(app) = app else {
        lines.push("  AX status: app accessibility snapshot unavailable".to_string());
        return lines;
    };

    let Some(window) = match_window(root, app) else {
        lines.push(format!(
            "  AX status: no matched AX window among {} AX windows",
            app.windows.len()
        ));
        for candidate in &app.windows {
            lines.push(format!(
                "    candidate title={:?} pos=({}, {}) size={}x{} score={}",
                candidate.title,
                candidate.x,
                candidate.y,
                candidate.width,
                candidate.height,
                match_score(root, candidate)
            ));
        }
        return lines;
    };

    lines.push(format!(
        "  AX match title={:?} pos=({}, {}) size={}x{} score={}",
        window.title,
        window.x,
        window.y,
        window.width,
        window.height,
        match_score(root, window)
    ));

    let mut visited = HashSet::new();
    visited.insert(element_ptr_key(&window.element));
    let mut visited_count = 1usize;
    collect_debug_descendant_lines(
        &window.element,
        AccessibilityQueryOptions {
            deep_children: true,
            relaxed_filtering: true,
        },
        1,
        &mut visited,
        &mut visited_count,
        &mut lines,
    );

    lines
}

fn match_window<'a>(
    root: &WindowInfo,
    app: &'a AccessibilityApp,
) -> Option<&'a AccessibilityWindow> {
    app.windows
        .iter()
        .min_by_key(|window| match_score(root, window))
        .filter(|window| match_score(root, window) < 20_000)
}

fn match_score(root: &WindowInfo, candidate: &AccessibilityWindow) -> i64 {
    candidate_match_score(
        root,
        candidate.title.trim(),
        candidate.x,
        candidate.y,
        candidate.width,
        candidate.height,
    )
}

fn candidate_match_score(
    root: &WindowInfo,
    candidate_title: &str,
    candidate_x: i32,
    candidate_y: i32,
    candidate_width: u32,
    candidate_height: u32,
) -> i64 {
    let mut score = 0;

    let root_title = root.title.trim();

    if !root_title.is_empty() && !candidate_title.is_empty() {
        if root_title == candidate_title {
            score += 0;
        } else if root_title.contains(candidate_title) || candidate_title.contains(root_title) {
            score += 250;
        } else {
            score += 10_000;
        }
    }

    score += (root.x - candidate_x).abs() as i64;
    score += (root.y - candidate_y).abs() as i64;
    score += (root.width as i64 - candidate_width as i64).abs();
    score += (root.height as i64 - candidate_height as i64).abs();

    score
}

fn collect_descendant_records(
    parent_id: u32,
    parent_element: &AXUIElement,
    pid: u32,
    app_name: &str,
    focused_element_ptr: Option<usize>,
    options: AccessibilityQueryOptions,
    depth: usize,
    visited: &mut HashSet<usize>,
    visited_count: &mut usize,
    next_id: &mut u32,
    records: &mut Vec<WindowInfoRecord>,
) {
    if depth >= max_ax_depth(options) || *visited_count >= MAX_AX_NODES_PER_WINDOW {
        return;
    }

    let children = ax_child_elements(parent_element, options);
    let sibling_count = children.len() as i32;

    for (index, child) in children.into_iter().enumerate() {
        let child_ptr = element_ptr_key(&child);
        if !visited.insert(child_ptr) {
            continue;
        }
        *visited_count += 1;
        if *visited_count > MAX_AX_NODES_PER_WINDOW {
            break;
        }

        let Some(info) = build_ax_info(
            &child,
            next_id,
            pid,
            app_name,
            sibling_count - index as i32 - 1,
            focused_element_ptr,
            options,
        ) else {
            if options.deep_children {
                collect_descendant_records(
                    parent_id,
                    &child,
                    pid,
                    app_name,
                    focused_element_ptr,
                    options,
                    depth + 1,
                    visited,
                    visited_count,
                    next_id,
                    records,
                );
            }
            continue;
        };

        let child_id = info.id;

        records.push(WindowInfoRecord {
            info,
            parent_id: Some(parent_id),
        });

        collect_descendant_records(
            child_id,
            &child,
            pid,
            app_name,
            focused_element_ptr,
            options,
            depth + 1,
            visited,
            visited_count,
            next_id,
            records,
        );
    }
}

fn collect_debug_descendant_lines(
    element: &AXUIElement,
    options: AccessibilityQueryOptions,
    depth: usize,
    visited: &mut HashSet<usize>,
    visited_count: &mut usize,
    lines: &mut Vec<String>,
) {
    if depth > max_ax_depth(options) || *visited_count >= MAX_AX_NODES_PER_WINDOW {
        return;
    }

    for child in ax_child_elements(element, options) {
        let child_ptr = element_ptr_key(&child);
        if !visited.insert(child_ptr) {
            lines.push(format!(
                "{}AX child skipped: cycle detected ptr=0x{child_ptr:x}",
                "  ".repeat(depth + 1),
            ));
            continue;
        }
        *visited_count += 1;
        if *visited_count > MAX_AX_NODES_PER_WINDOW {
            lines.push(format!(
                "{}AX child traversal stopped: node limit {} reached",
                "  ".repeat(depth + 1),
                MAX_AX_NODES_PER_WINDOW
            ));
            break;
        }

        let role = ax_string_attribute(&child, "AXRole").unwrap_or_default();
        let subrole = ax_string_attribute(&child, "AXSubrole").unwrap_or_default();
        let title = ax_string_attribute(&child, "AXTitle").unwrap_or_default();
        let identifier = ax_string_attribute(&child, "AXIdentifier").unwrap_or_default();
        let (x, y) = ax_position_attribute(&child).unwrap_or_default();
        let (width, height) = ax_size_attribute(&child).unwrap_or_default();
        let focused = ax_bool_attribute(&child, "AXFocused").unwrap_or(false);
        let minimized = ax_bool_attribute(&child, "AXMinimized").unwrap_or(false);

        lines.push(format!(
            "{}AX child role={:?} subrole={:?} title={:?} identifier={:?} pos=({}, {}) size={}x{} focused={} minimized={}",
            "  ".repeat(depth + 1),
            role,
            subrole,
            title,
            identifier,
            x,
            y,
            width,
            height,
            focused,
            minimized
        ));

        collect_debug_descendant_lines(&child, options, depth + 1, visited, visited_count, lines);
    }
}

fn build_ax_info(
    element: &AXUIElement,
    next_id: &mut u32,
    pid: u32,
    app_name: &str,
    z: i32,
    focused_element_ptr: Option<usize>,
    options: AccessibilityQueryOptions,
) -> Option<WindowInfo> {
    let title = ax_display_title(element);
    let role = ax_string_attribute(element, "AXRole").unwrap_or_default();
    let (x, y) = ax_position_attribute(element).unwrap_or_default();
    let (width, height) = ax_size_attribute(element).unwrap_or_default();

    let has_geometry = width > 0 || height > 0;
    let has_label = !title.is_empty() || !role.is_empty();

    if !has_geometry && !has_label {
        return None;
    }

    if !options.relaxed_filtering && !has_geometry {
        return None;
    }

    let is_minimized = ax_bool_attribute(element, "AXMinimized").unwrap_or(false);
    let is_focused = focused_element_ptr == Some(element_ptr_key(element))
        || ax_bool_attribute(element, "AXFocused").unwrap_or(false);

    Some(WindowInfo {
        id: next_synthetic_id(next_id),
        pid,
        app_name: app_name.to_string(),
        title,
        x,
        y,
        z,
        width,
        height,
        is_minimized,
        is_maximized: false,
        is_focused,
        children: Vec::new(),
    })
}

fn ax_display_title(element: &AXUIElement) -> String {
    if let Some(title) = ax_string_attribute(element, "AXTitle") {
        if !title.is_empty() {
            return title;
        }
    }

    if let Some(identifier) = ax_string_attribute(element, "AXIdentifier") {
        if !identifier.is_empty() {
            return identifier;
        }
    }

    let role = ax_string_attribute(element, "AXRole").unwrap_or_default();
    let subrole = ax_string_attribute(element, "AXSubrole").unwrap_or_default();

    if role.is_empty() {
        String::new()
    } else if subrole.is_empty() {
        role
    } else {
        format!("{role}:{subrole}")
    }
}

fn ax_copy_attribute_value(element: &AXUIElement, attribute: &str) -> Option<CFRetained<CFType>> {
    let attribute = CFString::from_str(attribute);
    let mut value = ptr::null();
    let error = unsafe { AXUIElementCopyAttributeValue(element, attribute.as_ref(), &mut value) };

    if error != AX_ERROR_SUCCESS || value.is_null() {
        return None;
    }

    let ptr = NonNull::new(value.cast_mut())?;
    Some(unsafe { CFRetained::from_raw(ptr.cast()) })
}

fn ax_string_attribute(element: &AXUIElement, attribute: &str) -> Option<String> {
    ax_copy_attribute_value(element, attribute)?
        .downcast::<CFString>()
        .ok()
        .map(|value| value.to_string())
}

fn ax_bool_attribute(element: &AXUIElement, attribute: &str) -> Option<bool> {
    ax_copy_attribute_value(element, attribute)?
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.value())
}

fn ax_position_attribute(element: &AXUIElement) -> Option<(i32, i32)> {
    let value = ax_copy_attribute_value(element, "AXPosition")?
        .downcast::<AXValue>()
        .ok()?;

    let mut point = CGPoint::default();
    let is_success = unsafe {
        AXValueGetValue(
            &value,
            AX_VALUE_TYPE_CGPOINT,
            (&mut point as *mut CGPoint).cast(),
        ) != 0
    };

    if !is_success {
        return None;
    }

    Some((point.x as i32, point.y as i32))
}

fn ax_size_attribute(element: &AXUIElement) -> Option<(u32, u32)> {
    let value = ax_copy_attribute_value(element, "AXSize")?
        .downcast::<AXValue>()
        .ok()?;

    if unsafe { AXValueGetType(&value) } != AX_VALUE_TYPE_CGSIZE {
        return None;
    }

    let mut size = CGSize::default();
    let is_success = unsafe {
        AXValueGetValue(
            &value,
            AX_VALUE_TYPE_CGSIZE,
            (&mut size as *mut CGSize).cast(),
        ) != 0
    };

    if !is_success {
        return None;
    }

    Some((size.width.max(0.0) as u32, size.height.max(0.0) as u32))
}

fn ax_array_attribute_elements(
    element: &AXUIElement,
    attribute: &str,
) -> Vec<CFRetained<AXUIElement>> {
    let Ok(array) = ax_copy_attribute_value(element, attribute)
        .ok_or(())
        .and_then(|value| value.downcast::<CFArray>().map_err(|_| ()))
    else {
        return Vec::new();
    };

    let len = unsafe { CFArrayGetCount(&array) }.max(0) as usize;
    let mut elements = Vec::new();

    for index in 0..len.min(MAX_AX_CHILDREN_PER_NODE) {
        let ptr = unsafe { CFArrayGetValueAtIndex(&array, index as isize) };
        if ptr.is_null() {
            continue;
        }

        let cf_type = unsafe { &*ptr.cast::<CFType>() };
        if let Some(element) = cf_type.downcast_ref::<AXUIElement>() {
            elements.push(element.retain());
        }
    }

    elements
}

fn max_ax_depth(options: AccessibilityQueryOptions) -> usize {
    if options.deep_children {
        DEEP_MAX_AX_DEPTH
    } else {
        BASE_MAX_AX_DEPTH
    }
}

fn child_attributes(options: AccessibilityQueryOptions) -> &'static [&'static str] {
    if options.deep_children {
        DEEP_CHILD_ATTRIBUTES
    } else {
        BASE_CHILD_ATTRIBUTES
    }
}

fn ax_child_elements(
    element: &AXUIElement,
    options: AccessibilityQueryOptions,
) -> Vec<CFRetained<AXUIElement>> {
    let mut children = Vec::new();
    let mut seen = HashSet::new();

    for attribute in child_attributes(options) {
        for child in ax_array_attribute_elements(element, attribute) {
            let key = element_ptr_key(&child);
            if seen.insert(key) {
                children.push(child);
            }
        }
    }

    children
}

pub(super) fn element_ptr_key(element: &AXUIElement) -> usize {
    element as *const AXUIElement as usize
}

fn next_synthetic_id(next_id: &mut u32) -> u32 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}

pub(super) fn accessibility_cache_for(
    windows: &[WindowInfo],
) -> HashMap<u32, Option<AccessibilityApp>> {
    let mut cache = HashMap::new();

    if !is_trusted() {
        log::warn!(
            "Accessibility permission not granted. Grant access in System Settings > Privacy & Security > Accessibility to query child UI elements."
        );
        return cache;
    }

    for window in windows {
        cache
            .entry(window.pid)
            .or_insert_with(|| application(window.pid));
    }

    cache
}

pub(super) fn debug_lines_for_windows(windows: &[WindowInfo]) -> Vec<String> {
    let mut lines = Vec::new();

    if !is_trusted() {
        lines.push(
            "Accessibility permission not granted. Enable System Settings > Privacy & Security > Accessibility for this process."
                .to_string(),
        );
        return lines;
    }

    let cache = accessibility_cache_for(windows);

    for window in windows {
        lines.extend(debug_lines_for_window(
            window,
            cache.get(&window.pid).and_then(|app| app.as_ref()),
        ));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct WindowMatchCandidate<'a> {
        title: &'a str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    }

    fn root_window(title: &str, x: i32, y: i32, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            id: 1,
            pid: 2,
            app_name: "app".to_string(),
            title: title.to_string(),
            x,
            y,
            z: 0,
            width,
            height,
            is_minimized: false,
            is_maximized: false,
            is_focused: false,
            children: Vec::new(),
        }
    }

    fn candidate_score(root: &WindowInfo, candidate: WindowMatchCandidate<'_>) -> i64 {
        candidate_match_score(
            root,
            candidate.title,
            candidate.x,
            candidate.y,
            candidate.width,
            candidate.height,
        )
    }

    #[test]
    fn test_match_score_prefers_exact_title_and_bounds() {
        let root = root_window("Editor", 10, 20, 800, 600);

        assert!(
            candidate_score(
                &root,
                WindowMatchCandidate {
                    title: "Editor",
                    x: 10,
                    y: 20,
                    width: 800,
                    height: 600,
                }
            ) < candidate_score(
                &root,
                WindowMatchCandidate {
                    title: "Other",
                    x: 10,
                    y: 20,
                    width: 800,
                    height: 600,
                }
            )
        );
    }

    #[test]
    fn test_cycle_detection_uses_pointer_keys() {
        let ptr = 0x12345usize;
        let mut visited = HashSet::new();

        assert!(visited.insert(ptr));
        assert!(!visited.insert(ptr));
    }

    #[test]
    fn test_ax_node_limit_is_positive() {
        assert!(MAX_AX_NODES_PER_WINDOW > 0);
        assert!(MAX_AX_NODES_PER_WINDOW >= MAX_AX_CHILDREN_PER_NODE);
    }

    #[test]
    fn test_deep_child_attributes_extend_base_attributes() {
        for attr in BASE_CHILD_ATTRIBUTES {
            assert!(DEEP_CHILD_ATTRIBUTES.contains(attr));
        }
        assert!(DEEP_CHILD_ATTRIBUTES.len() > BASE_CHILD_ATTRIBUTES.len());
    }

    #[test]
    fn test_next_synthetic_id_is_unique() {
        let mut next_id = 0x8000_0000;
        let first = next_synthetic_id(&mut next_id);
        let second = next_synthetic_id(&mut next_id);

        assert_ne!(first, second);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn test_max_ax_depth_respects_query_options() {
        assert_eq!(
            max_ax_depth(AccessibilityQueryOptions {
                deep_children: false,
                relaxed_filtering: false,
            }),
            BASE_MAX_AX_DEPTH
        );
        assert_eq!(
            max_ax_depth(AccessibilityQueryOptions {
                deep_children: true,
                relaxed_filtering: false,
            }),
            DEEP_MAX_AX_DEPTH
        );
    }
}
