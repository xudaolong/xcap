use std::{
    collections::HashSet,
    thread::sleep,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, RECT, RPC_E_CHANGED_MODE, S_FALSE},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Children,
        },
    },
    core::{BOOL, BSTR},
};

use crate::{
    XCapError,
    error::XCapResult,
    window::{WindowInfo, WindowInfoRecord, WindowQueryOptions},
};

const BASE_MAX_UIA_DEPTH: usize = 5;
const DEEP_MAX_UIA_DEPTH: usize = 8;
const MAX_UIA_NODES_PER_WINDOW: usize = 512;
const UIA_PROBE_INTERVAL_MS: u64 = 75;

#[derive(Debug, Clone, Copy)]
struct UiaQueryOptions {
    deep_children: bool,
    relaxed_filtering: bool,
    probe_timeout_ms: Option<u64>,
}

impl From<&WindowQueryOptions> for UiaQueryOptions {
    fn from(options: &WindowQueryOptions) -> Self {
        Self {
            deep_children: options.deep_children,
            relaxed_filtering: options.relaxed_filtering,
            probe_timeout_ms: options.probe_timeout_ms,
        }
    }
}

struct ComInitGuard {
    should_uninitialize: bool,
}

impl ComInitGuard {
    fn new() -> XCapResult<Self> {
        unsafe {
            let result = CoInitializeEx(None, COINIT_MULTITHREADED);

            if result.is_ok() {
                return Ok(Self {
                    should_uninitialize: true,
                });
            }

            if result == S_FALSE {
                return Ok(Self {
                    should_uninitialize: true,
                });
            }

            if result == RPC_E_CHANGED_MODE {
                return Ok(Self {
                    should_uninitialize: false,
                });
            }

            Err(XCapError::new(format!("CoInitializeEx failed: {result:?}")))
        }
    }
}

impl Drop for ComInitGuard {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn bstr_to_string(value: windows::core::Result<BSTR>) -> String {
    value
        .ok()
        .map(|value| value.to_string())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn rect_dimensions(rect: &RECT) -> Option<(i32, i32, u32, u32)> {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    if width <= 0 || height <= 0 {
        return None;
    }

    Some((rect.left, rect.top, width as u32, height as u32))
}

fn best_title(element: &IUIAutomationElement) -> String {
    let name = bstr_to_string(unsafe { element.CurrentName() });
    if !name.is_empty() {
        return name;
    }

    let automation_id = bstr_to_string(unsafe { element.CurrentAutomationId() });
    if !automation_id.is_empty() {
        return automation_id;
    }

    let class_name = bstr_to_string(unsafe { element.CurrentClassName() });
    if !class_name.is_empty() {
        return class_name;
    }

    bstr_to_string(unsafe { element.CurrentLocalizedControlType() })
}

fn node_key(element: &IUIAutomationElement, rect: Option<&RECT>) -> String {
    let hwnd = unsafe { element.CurrentNativeWindowHandle() }
        .ok()
        .map(|hwnd| hwnd.0 as usize)
        .unwrap_or_default();
    let automation_id = bstr_to_string(unsafe { element.CurrentAutomationId() });
    let class_name = bstr_to_string(unsafe { element.CurrentClassName() });
    let name = bstr_to_string(unsafe { element.CurrentName() });
    let (left, top, right, bottom) = rect
        .map(|rect| (rect.left, rect.top, rect.right, rect.bottom))
        .unwrap_or_default();

    format!(
        "{hwnd}|{automation_id}|{class_name}|{name}|{}|{}|{}|{}",
        left, top, right, bottom
    )
}

fn next_synthetic_id(next_id: &mut u32, used_ids: &mut HashSet<u32>) -> u32 {
    while used_ids.contains(next_id) {
        *next_id = next_id.wrapping_add(1);
    }

    let id = *next_id;
    used_ids.insert(id);
    *next_id = next_id.wrapping_add(1);
    id
}

fn should_skip_element(
    element: &IUIAutomationElement,
    rect: Option<&RECT>,
    root_hwnd: HWND,
    root_pid: u32,
    options: UiaQueryOptions,
    visited: &mut HashSet<String>,
) -> bool {
    let key = node_key(element, rect);
    if !visited.insert(key) {
        return true;
    }

    let is_offscreen = unsafe { element.CurrentIsOffscreen() }
        .unwrap_or(BOOL(0))
        .as_bool();
    if is_offscreen && !options.relaxed_filtering {
        return true;
    }

    let native_hwnd = unsafe { element.CurrentNativeWindowHandle() }.unwrap_or(HWND::default());
    if native_hwnd == root_hwnd {
        return true;
    }

    let pid = unsafe { element.CurrentProcessId() }.unwrap_or(root_pid as i32);
    if pid > 0 && pid as u32 != root_pid && !options.relaxed_filtering {
        return true;
    }

    false
}

fn max_uia_depth(options: UiaQueryOptions) -> usize {
    if options.deep_children {
        DEEP_MAX_UIA_DEPTH
    } else {
        BASE_MAX_UIA_DEPTH
    }
}

fn collect_children_recursive(
    automation: &IUIAutomation,
    element: &IUIAutomationElement,
    root_hwnd: HWND,
    root_pid: u32,
    root_app_name: &str,
    parent_id: u32,
    next_id: &mut u32,
    used_ids: &mut HashSet<u32>,
    visited: &mut HashSet<String>,
    records: &mut Vec<WindowInfoRecord>,
    options: UiaQueryOptions,
    depth: usize,
) -> XCapResult<()> {
    if depth >= max_uia_depth(options) || records.len() >= MAX_UIA_NODES_PER_WINDOW {
        return Ok(());
    }

    unsafe {
        let condition = automation.CreateTrueCondition()?;
        let children = element.FindAll(TreeScope_Children, &condition)?;
        let length = children.Length()?;
        let sibling_count = length.max(0);

        for index in 0..length {
            if records.len() >= MAX_UIA_NODES_PER_WINDOW {
                break;
            }

            let child = children.GetElement(index)?;
            let rect = child.CurrentBoundingRectangle().ok();

            if should_skip_element(&child, rect.as_ref(), root_hwnd, root_pid, options, visited) {
                continue;
            }

            let mut child_parent_id = parent_id;

            if let Some((x, y, width, height)) = rect.as_ref().and_then(rect_dimensions) {
                let id = next_synthetic_id(next_id, used_ids);
                let title = best_title(&child);
                let is_focused = child.CurrentHasKeyboardFocus().unwrap_or(BOOL(0)).as_bool();

                records.push(WindowInfoRecord {
                    info: WindowInfo {
                        id,
                        pid: root_pid,
                        app_name: root_app_name.to_string(),
                        title,
                        x,
                        y,
                        z: sibling_count - index - 1,
                        width,
                        height,
                        is_minimized: false,
                        is_maximized: false,
                        is_focused,
                        children: Vec::new(),
                    },
                    parent_id: Some(parent_id),
                });

                child_parent_id = id;
            } else if !options.deep_children {
                continue;
            }

            collect_children_recursive(
                automation,
                &child,
                root_hwnd,
                root_pid,
                root_app_name,
                child_parent_id,
                next_id,
                used_ids,
                visited,
                records,
                options,
                depth + 1,
            )?;
        }
    }

    Ok(())
}

pub(super) fn collect_uia_children(
    hwnd: HWND,
    parent_id: u32,
    root_pid: u32,
    root_app_name: &str,
    options: &WindowQueryOptions,
    next_id: &mut u32,
    used_ids: &mut HashSet<u32>,
    records: &mut Vec<WindowInfoRecord>,
) -> XCapResult<usize> {
    let _com_guard = ComInitGuard::new()?;
    let options = UiaQueryOptions::from(options);

    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let element = automation.ElementFromHandle(hwnd)?;
        let base_next_id = *next_id;
        let base_used_ids = used_ids.clone();
        let deadline = options
            .probe_timeout_ms
            .filter(|timeout_ms| *timeout_ms > 0)
            .map(|timeout_ms| Instant::now() + Duration::from_millis(timeout_ms));
        let mut best_records = Vec::new();
        let mut best_next_id = base_next_id;
        let mut best_used_ids = base_used_ids.clone();

        loop {
            let mut attempt_records = Vec::new();
            let mut attempt_next_id = base_next_id;
            let mut attempt_used_ids = base_used_ids.clone();
            let mut visited = HashSet::new();

            collect_children_recursive(
                &automation,
                &element,
                hwnd,
                root_pid,
                root_app_name,
                parent_id,
                &mut attempt_next_id,
                &mut attempt_used_ids,
                &mut visited,
                &mut attempt_records,
                options,
                0,
            )?;

            if attempt_records.len() > best_records.len() {
                best_records = attempt_records;
                best_next_id = attempt_next_id;
                best_used_ids = attempt_used_ids;
            }

            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    break;
                }

                sleep(Duration::from_millis(UIA_PROBE_INTERVAL_MS));
                continue;
            }

            break;
        }

        *next_id = best_next_id;
        *used_ids = best_used_ids;
        let added = best_records.len();
        records.extend(best_records);

        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_dimensions_rejects_empty_rects() {
        assert_eq!(
            rect_dimensions(&RECT {
                left: 10,
                top: 10,
                right: 10,
                bottom: 30,
            }),
            None
        );
    }

    #[test]
    fn test_rect_dimensions_returns_geometry() {
        assert_eq!(
            rect_dimensions(&RECT {
                left: 10,
                top: 20,
                right: 110,
                bottom: 220,
            }),
            Some((10, 20, 100, 200))
        );
    }

    #[test]
    fn test_next_synthetic_id_skips_used_values() {
        let mut next_id = 42;
        let mut used_ids = HashSet::from([42, 43]);

        assert_eq!(next_synthetic_id(&mut next_id, &mut used_ids), 44);
        assert_eq!(next_synthetic_id(&mut next_id, &mut used_ids), 45);
    }

    #[test]
    fn test_uia_limits_are_positive() {
        assert!(BASE_MAX_UIA_DEPTH > 0);
        assert!(DEEP_MAX_UIA_DEPTH >= BASE_MAX_UIA_DEPTH);
        assert!(MAX_UIA_NODES_PER_WINDOW > 0);
    }

    #[test]
    fn test_max_uia_depth_respects_deep_mode() {
        assert_eq!(
            max_uia_depth(UiaQueryOptions {
                deep_children: false,
                relaxed_filtering: false,
                probe_timeout_ms: None,
            }),
            BASE_MAX_UIA_DEPTH
        );
        assert_eq!(
            max_uia_depth(UiaQueryOptions {
                deep_children: true,
                relaxed_filtering: false,
                probe_timeout_ms: Some(200),
            }),
            DEEP_MAX_UIA_DEPTH
        );
    }
}
