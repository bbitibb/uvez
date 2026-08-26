use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HWND, LPARAM, RECT, SetLastError, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsWindow, IsWindowVisible,
    PostMessageW, SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPlacement, SetWindowPos, ShowWindow, WINDOW_LONG_PTR_INDEX, WINDOWPLACEMENT, WM_CLOSE,
    WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
    WS_THICKFRAME,
};
use windows::core::{BOOL, Result as WinResult};

pub(crate) const WINDOW_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MANAGED_STYLE_MASK: u32 =
    WS_CAPTION.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0 | WS_SYSMENU.0;
pub(crate) const MANAGED_EX_STYLE_MASK: u32 = WS_EX_DLGMODALFRAME.0
    | WS_EX_WINDOWEDGE.0
    | WS_EX_CLIENTEDGE.0
    | WS_EX_STATICEDGE.0
    | WS_EX_APPWINDOW.0;

#[derive(Clone, Default)]
pub(crate) struct WindowInfo {
    pub(crate) hwnd: HWND,
    pub(crate) title: String,
    pub(crate) pid: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowBounds {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

pub(crate) struct ManagedWindow {
    pub(crate) hwnd: HWND,
    pub(crate) pid: u32,
    pub(crate) title: String,
    pub(crate) exe_name: String,
    original_style: u32,
    original_ex_style: u32,
    original_rect: RECT,
    original_placement: WINDOWPLACEMENT,
    originally_visible: bool,
    managed_mode: bool,
}

fn get_window_attribute(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX) -> Result<u32, String> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let value = GetWindowLongPtrW(hwnd, index);
        let error = GetLastError();

        if value == 0 && error != ERROR_SUCCESS {
            Err(format!(
                "GetWindowLongPtrW({}) failed with Win32 error {}",
                index.0, error.0
            ))
        } else {
            Ok(value as u32)
        }
    }
}

fn set_window_attribute(
    hwnd: HWND,
    index: WINDOW_LONG_PTR_INDEX,
    value: u32,
) -> Result<(), String> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let previous = SetWindowLongPtrW(hwnd, index, value as isize);
        let error = GetLastError();

        if previous == 0 && error != ERROR_SUCCESS {
            Err(format!(
                "SetWindowLongPtrW({}) failed with Win32 error {}",
                index.0, error.0
            ))
        } else {
            Ok(())
        }
    }
}

impl ManagedWindow {
    pub(crate) fn is_open(&self) -> bool {
        let exists = unsafe { IsWindow(Some(self.hwnd)).as_bool() };
        exists && get_window_process_id(self.hwnd) == self.pid
    }

    pub(crate) fn enter_managed_mode(&mut self) -> Result<(), String> {
        if self.managed_mode {
            return Ok(());
        }

        if self.original_style & WS_CHILD.0 != 0 {
            return Err(format!(
                "refusing to manage child HWND {:?} as a top-level window",
                self.hwnd
            ));
        }

        self.hide();
        self.managed_mode = true;

        let apply_result = (|| {
            let current_style = get_window_attribute(self.hwnd, GWL_STYLE)?;
            let current_ex_style = get_window_attribute(self.hwnd, GWL_EXSTYLE)?;
            let managed_style = current_style & !MANAGED_STYLE_MASK;
            let managed_ex_style = (current_ex_style & !MANAGED_EX_STYLE_MASK) | WS_EX_TOOLWINDOW.0;

            set_window_attribute(self.hwnd, GWL_STYLE, managed_style)?;
            set_window_attribute(self.hwnd, GWL_EXSTYLE, managed_ex_style)?;

            unsafe {
                SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .map_err(|error| format!("could not apply borderless frame: {error}"))?;
            }

            let actual_style = get_window_attribute(self.hwnd, GWL_STYLE)?;
            let actual_ex_style = get_window_attribute(self.hwnd, GWL_EXSTYLE)?;
            if actual_style & WS_CHILD.0 != 0
                || actual_style & MANAGED_STYLE_MASK != 0
                || actual_ex_style & MANAGED_EX_STYLE_MASK != 0
                || actual_ex_style & WS_EX_TOOLWINDOW.0 == 0
            {
                return Err(format!(
                    "managed HWND {:?} rejected its borderless top-level configuration",
                    self.hwnd
                ));
            }

            Ok(())
        })();

        if let Err(error) = apply_result {
            if let Err(restore_error) = self.restore_native_state() {
                eprintln!("Could not roll back {}: {restore_error}", self.title);
            }
            return Err(error);
        }

        Ok(())
    }

    pub(crate) fn restore_native_state(&mut self) -> Result<(), String> {
        if !self.managed_mode {
            return Ok(());
        }

        if !self.is_open() {
            self.managed_mode = false;
            return Ok(());
        }

        self.hide();

        let current_style =
            get_window_attribute(self.hwnd, GWL_STYLE).unwrap_or(self.original_style);
        let current_ex_style =
            get_window_attribute(self.hwnd, GWL_EXSTYLE).unwrap_or(self.original_ex_style);
        let restored_style =
            (current_style & !MANAGED_STYLE_MASK) | (self.original_style & MANAGED_STYLE_MASK);
        let restored_ex_style = (current_ex_style & !(MANAGED_EX_STYLE_MASK | WS_EX_TOOLWINDOW.0))
            | (self.original_ex_style & (MANAGED_EX_STYLE_MASK | WS_EX_TOOLWINDOW.0));

        let style_result = set_window_attribute(self.hwnd, GWL_STYLE, restored_style);
        let ex_style_result = set_window_attribute(self.hwnd, GWL_EXSTYLE, restored_ex_style);
        let frame_result = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                self.original_rect.left,
                self.original_rect.top,
                self.original_rect.right - self.original_rect.left,
                self.original_rect.bottom - self.original_rect.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| format!("could not restore native frame: {error}"));
        let placement_result = unsafe { SetWindowPlacement(self.hwnd, &self.original_placement) }
            .map_err(|error| format!("could not restore native placement: {error}"));
        let original_z_band = if self.original_ex_style & WS_EX_TOPMOST.0 != 0 {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        let z_order_result = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(original_z_band),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| format!("could not restore native Z-order: {error}"));

        let restore_result = style_result
            .and(ex_style_result)
            .and(frame_result)
            .and(placement_result)
            .and(z_order_result);

        if restore_result.is_ok() {
            self.managed_mode = false;
            unsafe {
                let command = if self.originally_visible {
                    SHOW_WINDOW_CMD(self.original_placement.showCmd as i32)
                } else {
                    SW_HIDE
                };
                let _ = ShowWindow(self.hwnd, command);
            }
        }

        restore_result
    }

    pub(crate) fn position(&self, bounds: WindowBounds) -> WinResult<()> {
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
    }

    fn show(&self) {
        if !self.is_open() {
            return;
        }

        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    pub(crate) fn hide(&self) {
        if !self.is_open() {
            return;
        }

        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub(crate) fn activate(&self) {
        if !self.is_open() {
            return;
        }

        self.show();

        unsafe {
            if let Err(error) = BringWindowToTop(self.hwnd) {
                eprintln!("Could not raise {}: {error}", self.title);
            }

            if !SetForegroundWindow(self.hwnd).as_bool() {
                eprintln!("Windows refused to foreground {}", self.title);
            }

            let foreground = GetForegroundWindow();
            println!(
                "Foreground HWND after activation: {:?} (target {:?})",
                foreground, self.hwnd
            );
        }
    }

    pub(crate) fn close(&mut self) {
        if !self.is_open() {
            return;
        }

        self.hide();
        self.managed_mode = false;

        unsafe {
            if let Err(error) = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) {
                eprintln!("Could not close {}: {error}", self.title);
            }
        }
    }
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = lparam.0 as *mut Vec<WindowInfo>;

    unsafe {
        let title = get_window_title(hwnd);
        let pid = get_window_process_id(hwnd);

        if !title.is_empty() && IsWindowVisible(hwnd).as_bool() {
            (&mut *windows).push(WindowInfo { hwnd, title, pid });
        }
    }

    BOOL(1)
}

pub(crate) fn get_all_windows() -> WinResult<Vec<WindowInfo>> {
    let mut windows = Vec::new();
    let windows_ptr: *mut Vec<WindowInfo> = &mut windows;

    unsafe {
        EnumWindows(Some(enum_callback), LPARAM(windows_ptr as isize))?;
    }

    Ok(windows)
}

pub(crate) fn get_window_process_id(hwnd: HWND) -> u32 {
    let mut pid = 0;

    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }

    pid
}

pub(crate) fn format_title(raw: &str, fallback_exe: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        let fallback_path = Path::new(fallback_exe);
        if let Some(stem) = fallback_path.file_stem().and_then(|s| s.to_str())
            && !stem.is_empty()
        {
            return stem.to_string();
        }
        return fallback_exe.to_string();
    }

    let path = Path::new(trimmed);
    if let Some(extension) = path.extension()
        && extension.eq_ignore_ascii_case("exe")
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && !stem.is_empty()
    {
        return stem.to_string();
    }

    let is_absolute_path = (trimmed.len() >= 3
        && trimmed.as_bytes()[1] == b':'
        && (trimmed.as_bytes()[2] == b'\\' || trimmed.as_bytes()[2] == b'/'))
        || trimmed.starts_with(r"\\");

    if is_absolute_path
        && let Some(name) = path.file_name().and_then(|s| s.to_str())
        && !name.is_empty()
    {
        return name.to_string();
    }

    trimmed.to_string()
}

pub(crate) fn get_window_title(hwnd: HWND) -> String {
    unsafe {
        let text_len = GetWindowTextLengthW(hwnd);
        if text_len <= 0 {
            return String::new();
        }

        let mut buffer = vec![0u16; (text_len + 1) as usize];
        let written = GetWindowTextW(hwnd, &mut buffer);

        if written <= 0 {
            return String::new();
        }

        String::from_utf16_lossy(&buffer[..written as usize])
    }
}

fn launch_process(process_name: &str, args: &[&str]) -> std::io::Result<u32> {
    Command::new(process_name)
        .args(args)
        .spawn()
        .map(|child| child.id())
}

pub(crate) fn create_managed_window(
    process_name: &str,
    args: &[&str],
) -> std::result::Result<ManagedWindow, String> {
    let pid = launch_process(process_name, args)
        .map_err(|error| format!("failed to launch {process_name}: {error}"))?;
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;

    while Instant::now() < deadline {
        let windows =
            get_all_windows().map_err(|error| format!("failed to enumerate windows: {error}"))?;
        if let Some(info) = windows.into_iter().find(|window| window.pid == pid) {
            let original_style = get_window_attribute(info.hwnd, GWL_STYLE)?;
            let original_ex_style = get_window_attribute(info.hwnd, GWL_EXSTYLE)?;
            let mut original_rect = RECT::default();
            let mut original_placement = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };

            unsafe {
                GetWindowRect(info.hwnd, &mut original_rect).map_err(|error| {
                    format!("failed to capture original window bounds: {error}")
                })?;
                GetWindowPlacement(info.hwnd, &mut original_placement).map_err(|error| {
                    format!("failed to capture original window placement: {error}")
                })?;
            }

            return Ok(ManagedWindow {
                hwnd: info.hwnd,
                pid,
                title: format_title(&info.title, process_name),
                exe_name: process_name.to_string(),
                original_style,
                original_ex_style,
                original_rect,
                original_placement,
                originally_visible: unsafe { IsWindowVisible(info.hwnd).as_bool() },
                managed_mode: false,
            });
        }

        thread::sleep(Duration::from_millis(100));
    }

    Err(format!(
        "timed out waiting for a visible window from PID {pid}"
    ))
}
