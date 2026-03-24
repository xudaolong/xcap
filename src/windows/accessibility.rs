use std::collections::HashSet;

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
    window::{WindowInfo, WindowInfoRecord},
};

const MAX_UIA_DEPTH: usize = 5;
const MAX_UIA_NODES_PER_WINDOW: usize = 512;

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

fn node_key(element: &IUIAutomationElement, rect: &RECT) -> String {
    let hwnd = unsafe { element.CurrentNativeWindowHandle() }
        .ok()
        .map(|hwnd| hwnd.0 as usize)
        .unwrap_or_default();
    let automation_id = bstr_to_string(unsafe { element.CurrentAutomationId() });
    let class_name = bstr_to_string(unsafe { element.CurrentClassName() });
    let name = bstr_to_string(unsafe { element.CurrentName() });

    format!(
        "{hwnd}|{automation_id}|{class_name}|{name}|{}|{}|{}|{}",
        rect.left, rect.top, rect.right, rect.bottom
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
    rect: &RECT,
    root_hwnd: HWND,
    root_pid: u32,
    visited: &mut HashSet<String>,
) -> bool {
    let is_offscreen = unsafe { element.CurrentIsOffscreen() }
        .unwrap_or(BOOL(0))
        .as_bool();
    if is_offscreen {
        return true;
    }

    if rect_dimensions(rect).is_none() {
        return true;
    }

    let native_hwnd = unsafe { element.CurrentNativeWindowHandle() }.unwrap_or(HWND::default());
    if native_hwnd == root_hwnd {
        return true;
    }

    let pid = unsafe { element.CurrentProcessId() }.unwrap_or(root_pid as i32);
    if pid > 0 && pid as u32 != root_pid {
        return true;
    }

    let key = node_key(element, rect);
    !visited.insert(key)
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
    depth: usize,
) -> XCapResult<()> {
    if depth >= MAX_UIA_DEPTH || records.len() >= MAX_UIA_NODES_PER_WINDOW {
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
            let rect = match child.CurrentBoundingRectangle() {
                Ok(rect) => rect,
                Err(_) => continue,
            };

            if should_skip_element(&child, &rect, root_hwnd, root_pid, visited) {
                continue;
            }

            let Some((x, y, width, height)) = rect_dimensions(&rect) else {
                continue;
            };

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

            collect_children_recursive(
                automation,
                &child,
                root_hwnd,
                root_pid,
                root_app_name,
                id,
                next_id,
                used_ids,
                visited,
                records,
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
    next_id: &mut u32,
    used_ids: &mut HashSet<u32>,
    records: &mut Vec<WindowInfoRecord>,
) -> XCapResult<usize> {
    let _com_guard = ComInitGuard::new()?;

    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let element = automation.ElementFromHandle(hwnd)?;
        let start_len = records.len();
        let mut visited = HashSet::new();

        collect_children_recursive(
            &automation,
            &element,
            hwnd,
            root_pid,
            root_app_name,
            parent_id,
            next_id,
            used_ids,
            &mut visited,
            records,
            0,
        )?;

        Ok(records.len() - start_len)
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
        assert!(MAX_UIA_DEPTH > 0);
        assert!(MAX_UIA_NODES_PER_WINDOW > 0);
    }
}
