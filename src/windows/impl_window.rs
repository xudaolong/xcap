use core::slice;
use std::{collections::HashSet, ffi::c_void, mem, path::Path, ptr};

use image::RgbaImage;
use widestring::U16CString;
use windows::{
    Win32::{
        Foundation::{GetLastError, HANDLE, HWND, LPARAM, MAX_PATH, TRUE},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{IsRectEmpty, MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
        },
        Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
        System::{
            ProcessStatus::{GetModuleBaseNameW, GetModuleFileNameExW},
            Threading::{
                GetCurrentProcessId, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GW_CHILD, GW_HWNDNEXT, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow,
            GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
            WINDOW_EX_STYLE, WS_EX_TOOLWINDOW,
        },
    },
    core::{BOOL, HSTRING, PCWSTR},
};

use crate::{
    error::XCapResult,
    window::{
        WindowInfo, WindowInfoRecord, WindowQueryOptions, build_expanded_children,
        build_window_info_tree,
    },
};

use super::{
    accessibility::collect_uia_children,
    capture::capture_window,
    impl_monitor::ImplMonitor,
    utils::{get_window_bounds, open_process},
};

#[derive(Debug, Clone)]
pub(crate) struct ImplWindow {
    pub hwnd: HWND,
}

unsafe impl Send for ImplWindow {}
unsafe impl Sync for ImplWindow {}

fn is_window_cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut cloaked = 0u32;

        let is_dwm_get_window_attribute_fail = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            mem::size_of::<u32>() as u32,
        )
        .is_err();

        if is_dwm_get_window_attribute_fail {
            return false;
        }

        cloaked != 0
    }
}

// https://webrtc.googlesource.com/src.git/+/refs/heads/main/modules/desktop_capture/win/window_capture_utils.cc#52
fn is_valid_window(hwnd: HWND) -> bool {
    unsafe {
        // ignore invisible windows
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return false;
        }

        // 特别说明，与webrtc中源码有区别，子窗口也枚举进来，所以就不需要下面的代码了：
        // HWND owner = GetWindow(hwnd, GW_OWNER);
        // LONG exstyle = GetWindowLong(hwnd, GWL_EXSTYLE);
        // if (owner && !(exstyle & WS_EX_APPWINDOW)) {
        //   return TRUE;
        // }

        let mut lp_class_name = [0u16; MAX_PATH as usize];
        let lp_class_name_length = GetClassNameW(hwnd, &mut lp_class_name) as usize;
        if lp_class_name_length < 1 {
            return false;
        }

        let class_name = U16CString::from_vec_truncate(&lp_class_name[0..lp_class_name_length])
            .to_string()
            .unwrap_or_default();
        if class_name.is_empty() {
            return false;
        }

        let gwl_ex_style = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);

        // 过滤掉具有 WS_EX_TOOLWINDOW 样式的窗口
        if gwl_ex_style.contains(WS_EX_TOOLWINDOW) {
            let title = get_window_title(hwnd).unwrap_or_default();

            // windows 任务栏可以捕获
            if class_name.ne(&"Shell_TrayWnd") && title.is_empty() {
                return false;
            }
        }

        // GetWindowText* are potentially blocking operations if `hwnd` is
        // owned by the current process. The APIs will send messages to the window's
        // message loop, and if the message loop is waiting on this operation we will
        // enter a deadlock.
        // https://docs.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowtexta#remarks
        //
        // To help consumers avoid this, there is a DesktopCaptureOption to ignore
        // windows owned by the current process. Consumers should either ensure that
        // the thread running their message loop never waits on this operation, or use
        // the option to exclude these windows from the source list.
        let lp_dw_process_id = get_window_pid(hwnd);
        if lp_dw_process_id == GetCurrentProcessId() {
            return false;
        }

        // Skip Program Manager window.
        if class_name.eq("Progman") {
            return false;
        }
        // Skip Start button window on Windows Vista, Windows 7.
        // On Windows 8, Windows 8.1, Windows 10 Start button is not a top level
        // window, so it will not be examined here.
        if class_name.eq("Button") {
            return false;
        }

        if is_window_cloaked(hwnd) {
            return false;
        }

        if let Ok(rect) = get_window_bounds(hwnd) {
            if IsRectEmpty(&rect).as_bool() {
                return false;
            }
        } else {
            return false;
        }
    }

    true
}

extern "system" fn enum_valid_windows(hwnd: HWND, state: LPARAM) -> BOOL {
    unsafe {
        let state = Box::leak(Box::from_raw(state.0 as *mut Vec<HWND>));

        if is_valid_window(hwnd) {
            state.push(hwnd);
        }

        TRUE
    }
}

extern "system" fn enum_all_windows(hwnd: HWND, state: LPARAM) -> BOOL {
    unsafe {
        let state = Box::leak(Box::from_raw(state.0 as *mut Vec<HWND>));

        state.push(hwnd);

        TRUE
    }
}

fn get_window_title(hwnd: HWND) -> XCapResult<String> {
    unsafe {
        let text_length = GetWindowTextLengthW(hwnd);
        let mut wide_buffer = vec![0u16; (text_length + 1) as usize];
        GetWindowTextW(hwnd, &mut wide_buffer);
        let window_title = U16CString::from_vec_truncate(wide_buffer).to_string()?;

        Ok(window_title)
    }
}

fn child_window_filter_reason(hwnd: HWND) -> Option<&'static str> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return Some("not-visible-or-invalid");
        }

        if get_window_pid(hwnd) == GetCurrentProcessId() {
            return Some("current-process");
        }

        if is_window_cloaked(hwnd) {
            return Some("cloaked");
        }

        if let Ok(rect) = get_window_bounds(hwnd) {
            if IsRectEmpty(&rect).as_bool() {
                return Some("empty-bounds");
            }

            return None;
        }

        Some("bounds-unavailable")
    }
}

fn is_valid_child_window(hwnd: HWND) -> bool {
    child_window_filter_reason(hwnd).is_none()
}

fn child_hwnds(parent: HWND) -> Vec<HWND> {
    let mut hwnds = Vec::new();

    unsafe {
        let mut child = match GetWindow(parent, GW_CHILD) {
            Ok(hwnd) => hwnd,
            Err(_) => return hwnds,
        };

        while !child.0.is_null() {
            if is_valid_child_window(child) {
                hwnds.push(child);
            }

            child = match GetWindow(child, GW_HWNDNEXT) {
                Ok(hwnd) => hwnd,
                Err(_) => break,
            };
        }
    }

    hwnds
}

fn get_class_name(hwnd: HWND) -> String {
    unsafe {
        let mut lp_class_name = [0u16; MAX_PATH as usize];
        let len = GetClassNameW(hwnd, &mut lp_class_name) as usize;
        if len == 0 {
            return String::new();
        }

        U16CString::from_vec_truncate(&lp_class_name[..len])
            .to_string()
            .unwrap_or_default()
    }
}

fn all_child_hwnds(parent: HWND) -> Vec<HWND> {
    let mut hwnds = Vec::new();

    unsafe {
        let mut child = match GetWindow(parent, GW_CHILD) {
            Ok(hwnd) => hwnd,
            Err(_) => return hwnds,
        };

        while !child.0.is_null() {
            hwnds.push(child);
            child = match GetWindow(child, GW_HWNDNEXT) {
                Ok(hwnd) => hwnd,
                Err(_) => break,
            };
        }
    }

    hwnds
}

#[derive(Debug, Default)]
struct LangCodePage {
    pub w_language: u16,
    pub w_code_page: u16,
}

fn get_module_basename(handle: HANDLE) -> XCapResult<String> {
    unsafe {
        // 默认使用 module_basename
        let mut module_base_name_w = [0; MAX_PATH as usize];
        let result = GetModuleBaseNameW(handle, None, &mut module_base_name_w);

        if result == 0 {
            log::error!(
                "GetModuleBaseNameW({:?}) failed: {:?}",
                handle,
                GetLastError()
            );

            GetModuleFileNameExW(Some(handle), None, &mut module_base_name_w);
        }

        let module_basename = U16CString::from_vec_truncate(module_base_name_w).to_string()?;

        Ok(module_basename)
    }
}

fn get_process_image_path(handle: HANDLE) -> XCapResult<String> {
    unsafe {
        let mut filename = vec![0u16; MAX_PATH as usize];
        let mut size = filename.len() as u32;

        let is_success = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(filename.as_mut_ptr()),
            &mut size,
        )
        .is_ok();

        if is_success && size > 0 {
            filename.truncate(size as usize);
            return Ok(U16CString::from_vec_truncate(filename).to_string()?);
        }

        let mut fallback = [0u16; MAX_PATH as usize];
        let size = GetModuleFileNameExW(Some(handle), None, &mut fallback) as usize;
        if size > 0 {
            return Ok(U16CString::from_vec_truncate(&fallback[..size]).to_string()?);
        }

        Err(crate::XCapError::new(format!(
            "Get process image path failed: {:?}",
            GetLastError()
        )))
    }
}

fn basename_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .or_else(|| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn get_window_pid(hwnd: HWND) -> u32 {
    unsafe {
        let mut lp_dw_process_id = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut lp_dw_process_id));
        lp_dw_process_id
    }
}

fn get_app_name(pid: u32) -> XCapResult<String> {
    unsafe {
        let scope_guard_handle = match open_process(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(box_handle) => box_handle,
            Err(err) => {
                log::error!("open_process failed: {err}");
                return Ok(String::new());
            }
        };

        let image_path = match get_process_image_path(*scope_guard_handle) {
            Ok(path) if !path.is_empty() => path,
            Ok(_) => return get_module_basename(*scope_guard_handle),
            Err(err) => {
                log::debug!("get_process_image_path failed for pid {pid}: {err}");
                return get_module_basename(*scope_guard_handle);
            }
        };

        let filename = U16CString::from_str(&image_path)
            .map_err(|err| crate::XCapError::new(err.to_string()))?;
        let pcw_filename = PCWSTR::from_raw(filename.as_ptr());

        let file_version_info_size_w = GetFileVersionInfoSizeW(pcw_filename, None);
        if file_version_info_size_w == 0 {
            return Ok(basename_from_path(&image_path));
        }

        let mut file_version_info = vec![0u16; file_version_info_size_w as usize];

        GetFileVersionInfoW(
            pcw_filename,
            None,
            file_version_info_size_w,
            file_version_info.as_mut_ptr().cast(),
        )?;

        let mut lang_code_pages_ptr = ptr::null_mut();
        let mut lang_code_pages_length = 0;

        VerQueryValueW(
            file_version_info.as_ptr().cast(),
            &HSTRING::from("\\VarFileInfo\\Translation"),
            &mut lang_code_pages_ptr,
            &mut lang_code_pages_length,
        )
        .ok()?;

        let lang_code_pages: &[LangCodePage] =
            slice::from_raw_parts(lang_code_pages_ptr.cast(), lang_code_pages_length as usize);

        // 按照 keys 的顺序读取文件的属性值
        // 优先读取 FileDescription
        let keys = [
            "FileDescription",
            "ProductName",
            "ProductShortName",
            "InternalName",
            "OriginalFilename",
        ];

        for key in keys {
            for lang_code_page in lang_code_pages {
                let query_key = HSTRING::from(format!(
                    "\\StringFileInfo\\{:04x}{:04x}\\{}",
                    lang_code_page.w_language, lang_code_page.w_code_page, key
                ));

                let mut value_ptr = ptr::null_mut();
                let mut value_length: u32 = 0;

                let is_success = VerQueryValueW(
                    file_version_info.as_ptr().cast(),
                    &query_key,
                    &mut value_ptr,
                    &mut value_length,
                )
                .as_bool();

                if !is_success {
                    continue;
                }

                let value = slice::from_raw_parts(value_ptr.cast(), value_length as usize);
                let attr = U16CString::from_vec_truncate(value).to_string()?;
                let attr = attr.trim();

                if !attr.is_empty() {
                    return Ok(attr.to_string());
                }
            }
        }

        let basename = basename_from_path(&image_path);
        if basename.is_empty() {
            get_module_basename(*scope_guard_handle)
        } else {
            Ok(basename)
        }
    }
}

fn collect_window_record(
    hwnd: HWND,
    parent_id: Option<u32>,
    z: i32,
    options: &WindowQueryOptions,
    next_synthetic_id: &mut u32,
    used_ids: &mut HashSet<u32>,
    records: &mut Vec<WindowInfoRecord>,
) -> XCapResult<()> {
    let impl_window = ImplWindow::new(hwnd);
    let id = impl_window.id()?;
    used_ids.insert(id);
    let pid = impl_window.pid()?;
    let app_name = impl_window.app_name()?;

    records.push(WindowInfoRecord {
        info: WindowInfo {
            id,
            pid,
            app_name: app_name.clone(),
            title: impl_window.title()?,
            x: impl_window.x()?,
            y: impl_window.y()?,
            z,
            width: impl_window.width()?,
            height: impl_window.height()?,
            is_minimized: impl_window.is_minimized()?,
            is_maximized: impl_window.is_maximized()?,
            is_focused: impl_window.is_focused()?,
            children: Vec::new(),
        },
        parent_id,
    });

    if options.include_children {
        let children = child_hwnds(hwnd);
        let sibling_count = children.len() as i32;

        if children.is_empty() {
            if let Err(err) = collect_uia_children(
                hwnd,
                id,
                pid,
                &app_name,
                options,
                next_synthetic_id,
                used_ids,
                records,
            ) {
                log::debug!("collect_uia_children failed for {:?}: {err}", hwnd);
            }
        } else {
            for (index, child) in children.into_iter().enumerate() {
                collect_window_record(
                    child,
                    Some(id),
                    sibling_count - index as i32 - 1,
                    options,
                    next_synthetic_id,
                    used_ids,
                    records,
                )?;
            }
        }
    }

    Ok(())
}

impl ImplWindow {
    fn window_info(&self) -> XCapResult<WindowInfo> {
        Ok(WindowInfo {
            id: self.id()?,
            pid: self.pid()?,
            app_name: self.app_name()?,
            title: self.title()?,
            x: self.x()?,
            y: self.y()?,
            z: self.z()?,
            width: self.width()?,
            height: self.height()?,
            is_minimized: self.is_minimized()?,
            is_maximized: self.is_maximized()?,
            is_focused: self.is_focused()?,
            children: Vec::new(),
        })
    }

    fn new(hwnd: HWND) -> ImplWindow {
        ImplWindow { hwnd }
    }

    pub fn all() -> XCapResult<Vec<ImplWindow>> {
        let hwnds_mut_ptr: *mut Vec<HWND> = Box::into_raw(Box::default());

        let hwnds = unsafe {
            EnumWindows(Some(enum_valid_windows), LPARAM(hwnds_mut_ptr as isize))?;
            Box::from_raw(hwnds_mut_ptr)
        };

        let mut impl_windows = Vec::new();

        for &hwnd in hwnds.iter() {
            impl_windows.push(ImplWindow::new(hwnd));
        }

        Ok(impl_windows)
    }

    pub fn query(options: &WindowQueryOptions) -> XCapResult<Vec<WindowInfo>> {
        let hwnds_mut_ptr: *mut Vec<HWND> = Box::into_raw(Box::default());

        let hwnds = unsafe {
            EnumWindows(Some(enum_valid_windows), LPARAM(hwnds_mut_ptr as isize))?;
            Box::from_raw(hwnds_mut_ptr)
        };

        let mut records = Vec::new();
        let mut next_synthetic_id = 1u32;
        let mut used_ids = HashSet::new();
        let root_count = hwnds.len() as i32;

        for (index, &hwnd) in hwnds.iter().enumerate() {
            collect_window_record(
                hwnd,
                None,
                root_count - index as i32 - 1,
                options,
                &mut next_synthetic_id,
                &mut used_ids,
                &mut records,
            )?;
        }

        Ok(build_window_info_tree(records, options))
    }

    pub fn query_roots(options: &WindowQueryOptions) -> XCapResult<Vec<WindowInfo>> {
        let mut root_options = options.clone();
        root_options.include_children = false;
        Self::query(&root_options)
    }

    pub fn expand_children(
        window_id: u32,
        options: &WindowQueryOptions,
    ) -> XCapResult<Vec<WindowInfo>> {
        let hwnd = HWND(window_id as usize as *mut c_void);
        let root = ImplWindow::new(hwnd).window_info()?;
        let mut records = Vec::new();
        let mut next_synthetic_id = root.id.saturating_add(1).max(0x8000_0000);
        let mut used_ids = HashSet::from([root.id]);
        let children = child_hwnds(hwnd);

        if children.is_empty() {
            if let Err(err) = collect_uia_children(
                hwnd,
                root.id,
                root.pid,
                &root.app_name,
                options,
                &mut next_synthetic_id,
                &mut used_ids,
                &mut records,
            ) {
                log::debug!("collect_uia_children failed for {:?}: {err}", hwnd);
            }
        } else {
            let sibling_count = children.len() as i32;
            for (index, child) in children.into_iter().enumerate() {
                collect_window_record(
                    child,
                    Some(root.id),
                    sibling_count - index as i32 - 1,
                    options,
                    &mut next_synthetic_id,
                    &mut used_ids,
                    &mut records,
                )?;
            }
        }

        Ok(build_expanded_children(root, records, options))
    }

    pub fn debug_windows_children(options: &WindowQueryOptions) -> XCapResult<Vec<String>> {
        let hwnds_mut_ptr: *mut Vec<HWND> = Box::into_raw(Box::default());

        let hwnds = unsafe {
            EnumWindows(Some(enum_valid_windows), LPARAM(hwnds_mut_ptr as isize))?;
            Box::from_raw(hwnds_mut_ptr)
        };

        let mut lines = Vec::new();

        for &hwnd in hwnds.iter() {
            let impl_window = ImplWindow::new(hwnd);
            let title = impl_window.title().unwrap_or_default();
            let app = impl_window.app_name().unwrap_or_default();
            let bounds = get_window_bounds(hwnd).ok();
            let all_children = all_child_hwnds(hwnd);
            let kept_children = child_hwnds(hwnd);

            let top_level = format!(
                "top hwnd={:?} title={:?} app={:?} bounds={:?} include_children={} total_children={} kept_children={}",
                hwnd,
                title,
                app,
                bounds.map(|r| (r.left, r.top, r.right, r.bottom)),
                options.include_children,
                all_children.len(),
                kept_children.len()
            );
            lines.push(top_level);

            for child in all_children {
                let child_title = get_window_title(child).unwrap_or_default();
                let class_name = get_class_name(child);
                let bounds = get_window_bounds(child)
                    .ok()
                    .map(|r| (r.left, r.top, r.right, r.bottom));
                let reason = child_window_filter_reason(child);

                lines.push(format!(
                    "  child hwnd={:?} class={:?} title={:?} bounds={:?} kept={} reason={}",
                    child,
                    class_name,
                    child_title,
                    bounds,
                    reason.is_none(),
                    reason.unwrap_or("accepted")
                ));
            }
        }

        Ok(lines)
    }
}

impl ImplWindow {
    pub fn id(&self) -> XCapResult<u32> {
        Ok(self.hwnd.0 as u32)
    }

    pub fn pid(&self) -> XCapResult<u32> {
        let pid = get_window_pid(self.hwnd);
        Ok(pid)
    }

    pub fn app_name(&self) -> XCapResult<String> {
        get_app_name(self.pid()?)
    }

    pub fn title(&self) -> XCapResult<String> {
        get_window_title(self.hwnd)
    }

    pub fn current_monitor(&self) -> XCapResult<ImplMonitor> {
        let h_monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };

        Ok(ImplMonitor::new(h_monitor))
    }

    pub fn x(&self) -> XCapResult<i32> {
        let rect = get_window_bounds(self.hwnd)?;
        Ok(rect.left)
    }

    pub fn y(&self) -> XCapResult<i32> {
        let rect = get_window_bounds(self.hwnd)?;
        Ok(rect.top)
    }

    pub fn z(&self) -> XCapResult<i32> {
        let hwnds_mut_ptr: *mut Vec<HWND> = Box::into_raw(Box::default());

        let hwnds = unsafe {
            // EnumWindows 函数按照 Z 顺序遍历顶层窗口，从最顶层的窗口开始，依次向下遍历。
            EnumWindows(Some(enum_all_windows), LPARAM(hwnds_mut_ptr as isize))?;
            Box::from_raw(hwnds_mut_ptr)
        };

        let mut z = hwnds.len() as i32;
        for &hwnd in hwnds.iter() {
            z -= 1;
            if self.hwnd == hwnd {
                break;
            }
        }

        Ok(z)
    }

    pub fn width(&self) -> XCapResult<u32> {
        let rect = get_window_bounds(self.hwnd)?;
        Ok((rect.right - rect.left) as u32)
    }

    pub fn height(&self) -> XCapResult<u32> {
        let rect = get_window_bounds(self.hwnd)?;
        Ok((rect.bottom - rect.top) as u32)
    }

    pub fn is_minimized(&self) -> XCapResult<bool> {
        unsafe { Ok(IsIconic(self.hwnd).as_bool()) }
    }

    pub fn is_maximized(&self) -> XCapResult<bool> {
        unsafe { Ok(IsZoomed(self.hwnd).as_bool()) }
    }

    pub fn is_focused(&self) -> XCapResult<bool> {
        unsafe { Ok(GetForegroundWindow() == self.hwnd) }
    }

    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        capture_window(self)
    }
}

#[cfg(feature = "wgc")]
impl Drop for ImplWindow {
    fn drop(&mut self) {
        use super::wgc::WINDOW_GRAPHICS_CAPTURE_ITEM;

        if let Ok(mut monitor_items) = WINDOW_GRAPHICS_CAPTURE_ITEM.lock() {
            monitor_items.remove(&(self.hwnd.0 as usize));
        }
    }
}
