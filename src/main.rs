use std::ffi::c_void;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT, RECT, SetLastError, WPARAM,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_U,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SUBCLASSPROC, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsWindow, IsWindowVisible,
    MSG, PostMessageW, SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPlacement, SetWindowPos, ShowWindow, WINDOW_LONG_PTR_INDEX, WINDOWPLACEMENT,
    WM_CANCELMODE, WM_CLOSE, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_HOTKEY, WM_NCDESTROY,
    WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_TOPMOST,
    WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};
use windows::core::{BOOL, Result as WinResult};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

const SWITCH_HOTKEY_ID: i32 = 1;
const HOST_SUBCLASS_ID: usize = 1;
const WINDOW_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MANAGED_STYLE_MASK: u32 =
    WS_CAPTION.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0 | WS_SYSMENU.0;
const MANAGED_EX_STYLE_MASK: u32 =
    WS_EX_DLGMODALFRAME.0 | WS_EX_WINDOWEDGE.0 | WS_EX_CLIENTEDGE.0 | WS_EX_STATICEDGE.0;

#[derive(Clone, Default)]
struct WindowInfo {
    hwnd: HWND,
    title: String,
    pid: u32,
}

#[derive(Clone, Copy, Debug)]
struct WindowBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct ManagedWindow {
    hwnd: HWND,
    pid: u32,
    title: String,
    exe_name: String,
    original_style: u32,
    original_ex_style: u32,
    original_rect: RECT,
    original_placement: WINDOWPLACEMENT,
    originally_visible: bool,
    managed_mode: bool,
}

#[derive(Default)]
struct NativeHostEvents {
    active_hwnd: AtomicIsize,
    active_pid: AtomicU32,
    lifted_hwnd: AtomicIsize,
    lifted_pid: AtomicU32,
    lifted_was_topmost: AtomicBool,
    in_size_move: AtomicBool,
    size_move_finished: AtomicBool,
    subclass_ref_released: AtomicBool,
}

const HOST_SUBCLASS_PROC: SUBCLASSPROC = Some(host_subclass_proc);

fn hwnd_from_atomic(value: isize) -> HWND {
    HWND(value as *mut c_void)
}

fn hwnd_matches_pid(hwnd: HWND, expected_pid: u32) -> bool {
    expected_pid != 0 && get_window_process_id(hwnd) == expected_pid
}

fn lift_active_guest(events: &NativeHostEvents, host_hwnd: HWND) {
    if unsafe { GetForegroundWindow() } != host_hwnd {
        return;
    }

    let active_value = events.active_hwnd.load(Ordering::Acquire);
    let active_pid = events.active_pid.load(Ordering::Acquire);
    if active_value == 0 {
        return;
    }

    let active_hwnd = hwnd_from_atomic(active_value);
    if !hwnd_matches_pid(active_hwnd, active_pid) {
        return;
    }

    if !release_lifted_guest(events) {
        return;
    }

    let ex_style = unsafe { GetWindowLongPtrW(active_hwnd, GWL_EXSTYLE) as u32 };
    let was_topmost = ex_style & WS_EX_TOPMOST.0 != 0;
    let lifted = unsafe {
        SetWindowPos(
            active_hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };

    if lifted.is_ok() {
        events
            .lifted_was_topmost
            .store(was_topmost, Ordering::Release);
        events.lifted_pid.store(active_pid, Ordering::Release);
        events.lifted_hwnd.store(active_value, Ordering::Release);
    }
}

fn release_lifted_guest(events: &NativeHostEvents) -> bool {
    let lifted_value = events.lifted_hwnd.load(Ordering::Acquire);
    if lifted_value == 0 {
        return true;
    }

    let lifted_hwnd = hwnd_from_atomic(lifted_value);
    let lifted_pid = events.lifted_pid.load(Ordering::Acquire);
    if !hwnd_matches_pid(lifted_hwnd, lifted_pid) {
        let _ = events.lifted_hwnd.compare_exchange(
            lifted_value,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        return true;
    }

    let insert_after = if events.lifted_was_topmost.load(Ordering::Acquire) {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    let released = unsafe {
        SetWindowPos(
            lifted_hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };

    if released.is_ok() {
        let _ = events.lifted_hwnd.compare_exchange(
            lifted_value,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        true
    } else {
        false
    }
}

unsafe extern "system" fn host_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    if reference_data != 0 {
        let events = unsafe { &*(reference_data as *const NativeHostEvents) };

        match message {
            WM_ENTERSIZEMOVE => {
                events.size_move_finished.store(false, Ordering::Release);
                events.in_size_move.store(true, Ordering::Release);
                lift_active_guest(events, hwnd);
            }
            WM_EXITSIZEMOVE => {
                let _ = release_lifted_guest(events);
                events.in_size_move.store(false, Ordering::Release);
                events.size_move_finished.store(true, Ordering::Release);
            }
            WM_CANCELMODE => {
                let _ = release_lifted_guest(events);
                if events.in_size_move.swap(false, Ordering::AcqRel) {
                    events.size_move_finished.store(true, Ordering::Release);
                }
            }
            WM_NCDESTROY => {
                let _ = release_lifted_guest(events);
                events.in_size_move.store(false, Ordering::Release);
            }
            _ => {}
        }
    }

    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };

    if message == WM_NCDESTROY && reference_data != 0 {
        unsafe { release_subclass_reference(reference_data) };
    }

    result
}

unsafe fn release_subclass_reference(reference_data: usize) {
    let pointer = reference_data as *const NativeHostEvents;
    let events = unsafe { &*pointer };

    if !events.subclass_ref_released.swap(true, Ordering::AcqRel) {
        drop(unsafe { Arc::from_raw(pointer) });
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

impl ManagedWindow {
    fn is_open(&self) -> bool {
        let exists = unsafe { IsWindow(Some(self.hwnd)).as_bool() };
        exists && get_window_process_id(self.hwnd) == self.pid
    }

    fn enter_managed_mode(&mut self) -> Result<(), String> {
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
            let managed_ex_style = current_ex_style & !MANAGED_EX_STYLE_MASK;

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

    fn restore_native_state(&mut self) -> Result<(), String> {
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
        let restored_ex_style = (current_ex_style & !MANAGED_EX_STYLE_MASK)
            | (self.original_ex_style & MANAGED_EX_STYLE_MASK);

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

    fn position(&self, bounds: WindowBounds) -> WinResult<()> {
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

    fn hide(&self) {
        if !self.is_open() {
            return;
        }

        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn activate(&self) {
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

    fn close(&self) {
        if !self.is_open() {
            return;
        }

        unsafe {
            if let Err(error) = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) {
                eprintln!("Could not close {}: {error}", self.title);
            }
        }
    }
}

struct App {
    window: Option<Window>,
    managed_windows: Vec<ManagedWindow>,
    active: Option<usize>,
    bounds_dirty: bool,
    refocus_pending: bool,
    switch_requested: Arc<AtomicBool>,
    native_host_events: Arc<NativeHostEvents>,
    host_subclass_reference: Option<usize>,
    lift_release_attempts: u8,
    hotkey_registered: bool,
}

impl App {
    fn new(switch_requested: Arc<AtomicBool>) -> Self {
        Self {
            window: None,
            managed_windows: Vec::new(),
            active: None,
            bounds_dirty: false,
            refocus_pending: false,
            switch_requested,
            native_host_events: Arc::new(NativeHostEvents::default()),
            host_subclass_reference: None,
            lift_release_attempts: 0,
            hotkey_registered: false,
        }
    }

    fn add_managed_window(&mut self, managed: ManagedWindow) -> Result<(), String> {
        let index = self.managed_windows.len();
        self.managed_windows.push(managed);

        self.managed_windows[index].enter_managed_mode()?;
        let managed = &self.managed_windows[index];
        println!(
            "Managing {} (PID {}, HWND {:?}): {}",
            managed.exe_name, managed.pid, managed.hwnd, managed.title
        );
        Ok(())
    }

    fn hide_window(&mut self, index: usize) {
        if self.active == Some(index) {
            let _ = self.reconcile_move_lift();
        }

        if let Some(window) = self.managed_windows.get_mut(index) {
            window.hide();
        }
    }

    fn install_host_subclass(&mut self) -> Result<(), String> {
        if self.host_subclass_reference.is_some() {
            return Ok(());
        }

        let hwnd = self
            .host_hwnd()
            .ok_or_else(|| "Uvez host HWND is unavailable".to_string())?;
        self.native_host_events
            .subclass_ref_released
            .store(false, Ordering::Release);
        let reference_data = Arc::into_raw(Arc::clone(&self.native_host_events)) as usize;
        let installed = unsafe {
            SetWindowSubclass(hwnd, HOST_SUBCLASS_PROC, HOST_SUBCLASS_ID, reference_data)
        };

        if !installed.as_bool() {
            drop(unsafe { Arc::from_raw(reference_data as *const NativeHostEvents) });
            return Err("could not observe the Uvez native move/resize loop".to_string());
        }

        self.host_subclass_reference = Some(reference_data);
        Ok(())
    }

    fn remove_host_subclass(&mut self) {
        let Some(reference_data) = self.host_subclass_reference else {
            return;
        };

        if let Some(hwnd) = self.host_hwnd() {
            let removed =
                unsafe { RemoveWindowSubclass(hwnd, HOST_SUBCLASS_PROC, HOST_SUBCLASS_ID) };
            if removed.as_bool() {
                self.host_subclass_reference = None;
                unsafe { release_subclass_reference(reference_data) };
                return;
            }
        }

        if self
            .native_host_events
            .subclass_ref_released
            .load(Ordering::Acquire)
        {
            self.host_subclass_reference = None;
        } else {
            eprintln!("Could not remove the Uvez host window subclass safely");
        }
    }

    fn host_content_bounds(&self) -> Option<WindowBounds> {
        let window = self.window.as_ref()?;
        let position = window.inner_position().ok()?;
        let size = window.inner_size();

        if size.width == 0 || size.height == 0 {
            return None;
        }

        Some(WindowBounds {
            x: position.x,
            y: position.y,
            width: size.width.try_into().ok()?,
            height: size.height.try_into().ok()?,
        })
    }

    fn host_hwnd(&self) -> Option<HWND> {
        let handle = self.window.as_ref()?.window_handle().ok()?;

        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut c_void)),
            _ => None,
        }
    }

    fn host_is_foreground(&self) -> bool {
        self.host_hwnd()
            .is_some_and(|host| unsafe { GetForegroundWindow() == host })
    }

    fn group_is_foreground(&self) -> bool {
        let foreground = unsafe { GetForegroundWindow() };

        self.host_hwnd().is_some_and(|host| foreground == host)
            || self
                .active
                .and_then(|index| self.managed_windows.get(index))
                .is_some_and(|managed| managed.is_open() && foreground == managed.hwnd)
    }

    fn reconcile_move_lift(&mut self) -> bool {
        if release_lifted_guest(&self.native_host_events) {
            self.lift_release_attempts = 0;
            return true;
        }

        self.lift_release_attempts = self.lift_release_attempts.saturating_add(1);
        if self.lift_release_attempts == 1 {
            eprintln!("Could not restore the managed window's normal Z-order; retrying");
        }

        if self.lift_release_attempts <= 3 {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        } else if self.lift_release_attempts == 4 {
            eprintln!("Managed window Z-order restoration still failed after three retries");
        }

        false
    }

    fn raise_active_without_activation(&self) {
        let Some(active) = self.active else {
            return;
        };

        let managed = &self.managed_windows[active];
        if !managed.is_open() {
            return;
        }
        let result = unsafe {
            SetWindowPos(
                managed.hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };

        if let Err(error) = result {
            eprintln!(
                "Could not raise {} above the Uvez host: {error}",
                managed.title
            );
        }
    }

    fn raise_host_without_activation(&self) {
        let Some(hwnd) = self.host_hwnd() else {
            return;
        };

        let result = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };

        if let Err(error) = result {
            eprintln!("Could not raise the Uvez host: {error}");
        }
    }

    fn position_active_window(&self) -> bool {
        let (Some(active), Some(bounds)) = (self.active, self.host_content_bounds()) else {
            return false;
        };

        let managed = &self.managed_windows[active];
        if !managed.is_open() {
            eprintln!("Managed HWND {:?} is no longer valid", managed.hwnd);
            return false;
        }

        if let Err(error) = managed.position(bounds) {
            eprintln!("Could not position {}: {error}", managed.title);
            return false;
        }

        true
    }

    fn sync_group_bounds(&mut self) {
        self.bounds_dirty = !self.position_active_window();
    }

    fn update_host_title(&self) {
        let Some(window) = &self.window else {
            return;
        };

        let title = self
            .active
            .and_then(|index| self.managed_windows.get(index))
            .map(|managed| managed.title.as_str())
            .unwrap_or("No managed window");

        window.set_title(&format!("Uvez - {title}"));
    }

    fn activate_window(&mut self, index: usize) {
        if index >= self.managed_windows.len() || !self.managed_windows[index].is_open() {
            return;
        }

        if !self.reconcile_move_lift() {
            return;
        }

        for other in 0..self.managed_windows.len() {
            if other != index {
                self.hide_window(other);
            }
        }

        self.active = Some(index);
        self.native_host_events
            .active_pid
            .store(self.managed_windows[index].pid, Ordering::Release);
        self.native_host_events.active_hwnd.store(
            self.managed_windows[index].hwnd.0 as isize,
            Ordering::Release,
        );
        self.bounds_dirty = false;
        let can_show = self.position_active_window();

        {
            let managed = &self.managed_windows[index];
            println!(
                "Activating tab {}: {} (PID {}, HWND {:?})",
                index + 1,
                managed.title,
                managed.pid,
                managed.hwnd
            );
        }
        if can_show {
            self.raise_host_without_activation();
            self.managed_windows[index].activate();
        } else {
            self.managed_windows[index].hide();
        }

        self.refocus_pending = false;
        self.update_host_title();
    }

    fn activate_next_window(&mut self) {
        if self.managed_windows.is_empty() {
            return;
        }

        let first_candidate = self
            .active
            .map(|active| (active + 1) % self.managed_windows.len())
            .unwrap_or(0);

        for offset in 0..self.managed_windows.len() {
            let candidate = (first_candidate + offset) % self.managed_windows.len();
            if self.managed_windows[candidate].is_open() {
                self.activate_window(candidate);
                return;
            }
        }
    }

    fn reactivate_active_window(&mut self) {
        if !self.position_active_window() {
            return;
        }

        if let Some(active) = self.active {
            self.managed_windows[active].activate();
        }
    }

    fn close_managed_window(&mut self, index: usize) {
        if self.active == Some(index) {
            let _ = self.reconcile_move_lift();
        }

        if let Some(managed) = self.managed_windows.get_mut(index) {
            if let Err(error) = managed.restore_native_state() {
                eprintln!(
                    "Could not restore {} before closing: {error}",
                    managed.title
                );
            }
            managed.close();
        }
    }

    fn close_all_managed_windows(&mut self) {
        for index in 0..self.managed_windows.len() {
            self.close_managed_window(index);
        }
    }

    fn reveal_managed_windows(&mut self) {
        let _ = self.reconcile_move_lift();

        for managed in &mut self.managed_windows {
            if let Err(error) = managed.restore_native_state() {
                eprintln!("Could not release {}: {error}", managed.title);
            }
        }

        if let Some(managed) = self.managed_windows.first_mut() {
            managed.activate();
        }
    }

    fn release_managed_windows(&mut self) {
        let _ = self.reconcile_move_lift();

        for managed in &mut self.managed_windows {
            if let Err(error) = managed.restore_native_state() {
                eprintln!("Could not restore {} while exiting: {error}", managed.title);
            }
        }
    }

    fn register_switch_hotkey(&mut self) {
        let result = unsafe {
            RegisterHotKey(
                None,
                SWITCH_HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                u32::from(VK_U.0),
            )
        };

        match result {
            Ok(()) => {
                self.hotkey_registered = true;
                println!("Press Ctrl+Alt+U to switch the active managed window");
            }
            Err(error) => eprintln!("Could not register Ctrl+Alt+U: {error}"),
        }
    }

    fn unregister_switch_hotkey(&mut self) {
        if !self.hotkey_registered {
            return;
        }

        if let Err(error) = unsafe { UnregisterHotKey(None, SWITCH_HOTKEY_ID) } {
            eprintln!("Could not unregister Ctrl+Alt+U: {error}");
        }
        self.hotkey_registered = false;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = Window::default_attributes().with_title("Uvez");
        self.window = Some(event_loop.create_window(attributes).unwrap());

        if let Err(error) = self.install_host_subclass() {
            eprintln!("Could not initialize native host synchronization: {error}");
            event_loop.exit();
            return;
        }

        for _ in 0..2 {
            match create_managed_window("alacritty.exe", &["--print-events"]) {
                Ok(managed) => {
                    if let Err(error) = self.add_managed_window(managed) {
                        eprintln!("Could not manage window: {error}");
                        self.reveal_managed_windows();
                        event_loop.exit();
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("Could not create managed window: {error}");
                    self.reveal_managed_windows();
                    event_loop.exit();
                    return;
                }
            }
        }

        self.register_switch_hotkey();
        self.activate_window(0);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.reconcile_move_lift();
                self.native_host_events
                    .active_hwnd
                    .store(0, Ordering::Release);
                self.native_host_events
                    .active_pid
                    .store(0, Ordering::Release);
                self.active = None;
                self.bounds_dirty = false;
                self.refocus_pending = false;
                self.switch_requested.store(false, Ordering::Release);
                self.close_all_managed_windows();
                event_loop.exit();
            }

            WindowEvent::Focused(true) => {
                self.raise_active_without_activation();
                self.sync_group_bounds();
                self.refocus_pending = true;
            }

            WindowEvent::Focused(false) => {
                let _ = self.reconcile_move_lift();
                if !self.group_is_foreground() {
                    self.refocus_pending = false;
                }
            }

            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let host_is_foreground = self.host_is_foreground();
                self.sync_group_bounds();
                self.refocus_pending = host_is_foreground;
            }

            WindowEvent::Resized(size) => {
                if size.width == 0 || size.height == 0 {
                    if let Some(active) = self.active {
                        self.hide_window(active);
                    }
                    self.bounds_dirty = false;
                    self.refocus_pending = false;
                } else {
                    let host_is_foreground = self.host_is_foreground();
                    self.sync_group_bounds();
                    self.refocus_pending = host_is_foreground;
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let move_finished = self
            .native_host_events
            .size_move_finished
            .swap(false, Ordering::AcqRel);
        let in_size_move = self.native_host_events.in_size_move.load(Ordering::Acquire);
        let lift_pending = self.native_host_events.lifted_hwnd.load(Ordering::Acquire) != 0;
        let lift_released = if !in_size_move && (move_finished || lift_pending) {
            self.reconcile_move_lift()
        } else {
            true
        };

        if move_finished {
            self.sync_group_bounds();
            self.refocus_pending = lift_released && self.group_is_foreground();
        }

        if self.switch_requested.swap(false, Ordering::AcqRel) {
            self.activate_next_window();
        }

        if self.bounds_dirty {
            self.sync_group_bounds();
        }

        if self.refocus_pending && !in_size_move {
            if self.group_is_foreground() {
                self.reactivate_active_window();
            }
            self.refocus_pending = false;
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let _ = self.reconcile_move_lift();
        self.native_host_events
            .active_hwnd
            .store(0, Ordering::Release);
        self.native_host_events
            .active_pid
            .store(0, Ordering::Release);
        self.release_managed_windows();
        self.unregister_switch_hotkey();
        self.remove_host_subclass();
    }
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = lparam.0 as *mut Vec<WindowInfo>;

    unsafe {
        let text_len = GetWindowTextLengthW(hwnd);
        let mut buffer = vec![0u16; (text_len + 1) as usize];
        let written = GetWindowTextW(hwnd, &mut buffer);
        let title = String::from_utf16_lossy(&buffer[..written as usize]);
        let pid = get_window_process_id(hwnd);

        if !title.is_empty() && IsWindowVisible(hwnd).as_bool() {
            (&mut *windows).push(WindowInfo { hwnd, title, pid });
        }
    }

    BOOL(1)
}

fn get_all_windows() -> WinResult<Vec<WindowInfo>> {
    let mut windows = Vec::new();
    let windows_ptr: *mut Vec<WindowInfo> = &mut windows;

    unsafe {
        EnumWindows(Some(enum_callback), LPARAM(windows_ptr as isize))?;
    }

    Ok(windows)
}

fn get_window_process_id(hwnd: HWND) -> u32 {
    let mut pid = 0;

    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }

    pid
}

fn launch_process(process_name: &str, args: &[&str]) -> std::io::Result<u32> {
    Command::new(process_name)
        .args(args)
        .spawn()
        .map(|child| child.id())
}

fn create_managed_window(
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
                title: info.title,
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

fn main() -> WinResult<()> {
    let switch_requested = Arc::new(AtomicBool::new(false));
    let hook_switch_requested = Arc::clone(&switch_requested);

    let mut event_loop_builder = EventLoop::builder();
    event_loop_builder.with_msg_hook(move |message| {
        let message = unsafe { &*message.cast::<MSG>() };
        if message.message == WM_HOTKEY && message.wParam.0 == SWITCH_HOTKEY_ID as usize {
            hook_switch_requested.store(true, Ordering::Release);
            true
        } else {
            false
        }
    });

    let event_loop = event_loop_builder.build().unwrap();
    let mut app = App::new(switch_requested);
    event_loop.run_app(&mut app).unwrap();

    Ok(())
}
