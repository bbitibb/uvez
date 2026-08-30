use crate::debug_log;
use std::ffi::c_void;
use std::panic;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetLastError, HWND, LPARAM, RECT, SetLastError, WPARAM,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GetClassNameW,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST,
    IsWindow, IsWindowVisible, PostMessageW, SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow, WINDOW_LONG_PTR_INDEX,
    WINDOWPLACEMENT, WM_APP, WM_CLOSE, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_WINDOWEDGE,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};
use windows::core::{BOOL, PWSTR, Result as WinResult};

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
    original_owner: isize,
    original_rect: RECT,
    original_placement: WINDOWPLACEMENT,
    originally_visible: bool,
    managed_mode: bool,
}

pub(crate) type ManagedWindowArrival = std::result::Result<DiscoveredWindow, String>;

pub(crate) struct DiscoveredWindow {
    hwnd: isize,
    pid: u32,
    title: String,
    exe_name: String,
    original_style: u32,
    original_ex_style: u32,
    original_owner: isize,
    original_rect: RECT,
    original_placement: WINDOWPLACEMENT,
    originally_visible: bool,
}

impl DiscoveredWindow {
    pub(crate) fn into_managed_window(self) -> ManagedWindow {
        ManagedWindow {
            hwnd: HWND(self.hwnd as *mut c_void),
            pid: self.pid,
            title: self.title,
            exe_name: self.exe_name,
            original_style: self.original_style,
            original_ex_style: self.original_ex_style,
            original_owner: self.original_owner,
            original_rect: self.original_rect,
            original_placement: self.original_placement,
            originally_visible: self.originally_visible,
            managed_mode: false,
        }
    }
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

fn set_window_owner(hwnd: HWND, owner: isize) -> Result<(), String> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let previous = SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner);
        let error = GetLastError();

        if previous == 0 && error != ERROR_SUCCESS {
            Err(format!(
                "SetWindowLongPtrW(GWLP_HWNDPARENT) failed with Win32 error {}",
                error.0
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

    pub(crate) fn enter_managed_mode(&mut self, owner: HWND) -> Result<(), String> {
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
            let managed_ex_style = current_ex_style & !(MANAGED_EX_STYLE_MASK | WS_EX_TOOLWINDOW.0);

            set_window_owner(self.hwnd, owner.0 as isize)?;
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
            let actual_owner = unsafe { GetWindowLongPtrW(self.hwnd, GWLP_HWNDPARENT) };
            if actual_style & WS_CHILD.0 != 0
                || actual_style & MANAGED_STYLE_MASK != 0
                || actual_ex_style & MANAGED_EX_STYLE_MASK != 0
                || actual_ex_style & WS_EX_TOOLWINDOW.0 != 0
                || actual_owner != owner.0 as isize
            {
                return Err(format!(
                    "managed HWND {:?} rejected its owned borderless configuration",
                    self.hwnd
                ));
            }

            Ok(())
        })();

        if let Err(error) = apply_result {
            if let Err(restore_error) = self.restore_native_state() {
                debug_log!("Could not roll back {}: {restore_error}", self.title);
            }
            return Err(error);
        }

        lock_restore_registry().push(RestoreRecord {
            hwnd: self.hwnd.0 as isize,
            original_style: self.original_style,
            original_ex_style: self.original_ex_style,
            original_owner: self.original_owner,
            original_rect: self.original_rect,
            original_placement: self.original_placement,
            originally_topmost: self.original_ex_style & WS_EX_TOPMOST.0 != 0,
            originally_visible: self.originally_visible,
        });

        Ok(())
    }

    pub(crate) fn restore_native_state(&mut self) -> Result<(), String> {
        if !self.managed_mode {
            return Ok(());
        }

        if !self.is_open() {
            self.managed_mode = false;
            unregister_panic_restore(self.hwnd);
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
        let owner_result = set_window_owner(self.hwnd, self.original_owner);
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
            .and(owner_result)
            .and(frame_result)
            .and(placement_result)
            .and(z_order_result);

        if restore_result.is_ok() {
            self.managed_mode = false;
            unregister_panic_restore(self.hwnd);
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
                debug_log!("Could not raise {}: {error}", self.title);
            }

            if !SetForegroundWindow(self.hwnd).as_bool() {
                debug_log!("Windows refused to foreground {}", self.title);
            }

            let foreground = GetForegroundWindow();
            debug_log!(
                "Foreground HWND after activation: {:?} (target {:?})",
                foreground,
                self.hwnd
            );
        }
    }

    pub(crate) fn close(&mut self) {
        if !self.is_open() {
            return;
        }

        self.hide();

        unsafe {
            if let Err(error) = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) {
                debug_log!("Could not close {}: {error}", self.title);
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

pub(crate) fn exe_path_for_process(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut path_buffer = [0u16; 1024];
        let mut length = u32::try_from(path_buffer.len()).unwrap_or(0);
        let queried = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path_buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        queried.ok()?;

        if length == 0 || length as usize > path_buffer.len() {
            return None;
        }

        Some(String::from_utf16_lossy(&path_buffer[..length as usize]))
    }
}

fn is_shell_surface(hwnd: HWND) -> bool {
    const SHELL_SURFACE_CLASSES: [&str; 6] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "Windows.UI.Core.CoreWindow",
        "XamlExplorerHostIslandWindow",
    ];

    unsafe {
        let mut buffer = [0u16; 64];
        let length = GetClassNameW(hwnd, &mut buffer);
        if length <= 0 {
            return false;
        }

        let class = String::from_utf16_lossy(&buffer[..length as usize]);
        SHELL_SURFACE_CLASSES.contains(&class.as_str())
    }
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

fn launch_process(process_name: &str, args: &[String]) -> std::io::Result<u32> {
    Command::new(process_name)
        .args(args)
        .spawn()
        .map(|child| child.id())
}

struct RestoreRecord {
    hwnd: isize,
    original_style: u32,
    original_ex_style: u32,
    original_owner: isize,
    original_rect: RECT,
    original_placement: WINDOWPLACEMENT,
    originally_topmost: bool,
    originally_visible: bool,
}

static RESTORE_REGISTRY: Mutex<Vec<RestoreRecord>> = Mutex::new(Vec::new());

fn lock_restore_registry() -> MutexGuard<'static, Vec<RestoreRecord>> {
    RESTORE_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn install_panic_restore_hook() {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_registered_windows();
        previous_hook(info);
    }));
}

fn restore_registered_windows() {
    let records = std::mem::take(&mut *lock_restore_registry());

    for record in records {
        restore_record(&record);
    }
}

fn restore_record(record: &RestoreRecord) {
    let hwnd = HWND(record.hwnd as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }

    let insertion_band = if record.originally_topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };

    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, record.original_style as isize);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, record.original_ex_style as isize);
        let _ = SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, record.original_owner);
        let _ = SetWindowPos(
            hwnd,
            None,
            record.original_rect.left,
            record.original_rect.top,
            record.original_rect.right - record.original_rect.left,
            record.original_rect.bottom - record.original_rect.top,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = SetWindowPlacement(hwnd, &record.original_placement);
        let _ = SetWindowPos(
            hwnd,
            Some(insertion_band),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let command = if record.originally_visible {
            SHOW_WINDOW_CMD(record.original_placement.showCmd as i32)
        } else {
            SW_HIDE
        };
        let _ = ShowWindow(hwnd, command);
    }
}

fn unregister_panic_restore(hwnd: HWND) {
    let value = hwnd.0 as isize;
    lock_restore_registry().retain(|record| record.hwnd != value);
}

pub(crate) fn request_managed_window(
    process_name: &str,
    args: &[&str],
    sender: mpsc::Sender<ManagedWindowArrival>,
    host_hwnd: Option<isize>,
) {
    let process_name = process_name.to_string();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();

    thread::spawn(move || {
        let arrival = discover_managed_window(&process_name, &args);
        let _ = sender.send(arrival);

        if let Some(hwnd) = host_hwnd {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_APP,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
}

fn discover_managed_window(process_name: &str, args: &[String]) -> ManagedWindowArrival {
    let pid = launch_process(process_name, args)
        .map_err(|error| format!("failed to launch {process_name}: {error}"))?;
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;

    while Instant::now() < deadline {
        let windows =
            get_all_windows().map_err(|error| format!("failed to enumerate windows: {error}"))?;
        if let Some(info) = windows.into_iter().find(|window| window.pid == pid) {
            return capture_window_state(info, process_name);
        }

        thread::sleep(Duration::from_millis(15));
    }

    Err(format!(
        "timed out waiting for a visible window from PID {pid}"
    ))
}

fn capture_window_state(info: WindowInfo, fallback_exe: &str) -> ManagedWindowArrival {
    let original_style = get_window_attribute(info.hwnd, GWL_STYLE)?;
    if original_style & WS_CHILD.0 != 0 {
        return Err(format!("refusing to attach child window {:?}", info.hwnd));
    }

    let original_ex_style = get_window_attribute(info.hwnd, GWL_EXSTYLE)?;
    let mut original_rect = RECT::default();
    let mut original_placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };

    unsafe {
        GetWindowRect(info.hwnd, &mut original_rect)
            .map_err(|error| format!("failed to capture original window bounds: {error}"))?;
        GetWindowPlacement(info.hwnd, &mut original_placement)
            .map_err(|error| format!("failed to capture original window placement: {error}"))?;
    }

    let originally_visible = unsafe { IsWindowVisible(info.hwnd).as_bool() };
    if originally_visible {
        unsafe {
            let _ = ShowWindow(info.hwnd, SW_HIDE);
        }
    }

    Ok(DiscoveredWindow {
        hwnd: info.hwnd.0 as isize,
        pid: info.pid,
        title: format_title(&info.title, fallback_exe),
        exe_name: fallback_exe.to_string(),
        original_style,
        original_ex_style,
        original_owner: unsafe { GetWindowLongPtrW(info.hwnd, GWLP_HWNDPARENT) },
        original_rect,
        original_placement,
        originally_visible,
    })
}

pub(crate) fn request_adopt_window(
    hwnd_value: isize,
    sender: mpsc::Sender<ManagedWindowArrival>,
    host_hwnd: Option<isize>,
) {
    thread::spawn(move || {
        let arrival = adopt_existing_window(hwnd_value);
        let _ = sender.send(arrival);

        if let Some(hwnd) = host_hwnd {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_APP,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
}

fn adopt_existing_window(hwnd_value: isize) -> ManagedWindowArrival {
    let hwnd = HWND(hwnd_value as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err("the window to attach no longer exists".to_string());
    }

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err("the window to attach is not visible".to_string());
    }

    if is_shell_surface(hwnd) {
        return Err("refusing to attach a shell surface".to_string());
    }

    let pid = get_window_process_id(hwnd);
    if pid == 0 {
        return Err("could not identify the owning process of the window to attach".to_string());
    }

    let exe_name = exe_path_for_process(pid)
        .map(|path| format_title(&path, "app"))
        .unwrap_or_else(|| "app".to_string());
    let title = get_window_title(hwnd);

    capture_window_state(WindowInfo { hwnd, title, pid }, &exe_name)
}

#[cfg(test)]
mod tests {
    use super::format_title;

    #[test]
    fn plain_title_passes_through() {
        assert_eq!(format_title("My Terminal", "alacritty.exe"), "My Terminal");
    }

    #[test]
    fn title_is_trimmed() {
        assert_eq!(format_title("  Hello  ", "alacritty.exe"), "Hello");
    }

    #[test]
    fn empty_title_falls_back_to_exe_stem() {
        assert_eq!(format_title("", "alacritty.exe"), "alacritty");
        assert_eq!(format_title("   ", "alacritty.exe"), "alacritty");
    }

    #[test]
    fn exe_path_reduces_to_stem() {
        assert_eq!(
            format_title("C:\\tools\\alacritty.exe", "alacritty.exe"),
            "alacritty"
        );
        assert_eq!(format_title("ALACRITTY.EXE", "alacritty.exe"), "ALACRITTY");
    }

    #[test]
    fn absolute_non_exe_path_reduces_to_file_name() {
        assert_eq!(
            format_title("C:\\Users\\me\\Documents\\report.txt", "alacritty.exe"),
            "report.txt"
        );
    }

    #[test]
    fn relative_paths_pass_through() {
        assert_eq!(format_title("notes.txt", "alacritty.exe"), "notes.txt");
    }

    #[test]
    fn unc_paths_reduce_to_file_name() {
        assert_eq!(format_title("\\\\server\\share\\app.exe", "x.exe"), "app");
    }
}
