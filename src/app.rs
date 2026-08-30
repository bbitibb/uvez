use std::ffi::c_void;
use std::sync::{
    Arc,
    atomic::{AtomicIsize, AtomicU32, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use crate::debug_log;
use crate::guest::{self, ManagedWindow, WindowBounds};
use crate::host_events::{
    self, HOST_SUBCLASS_ID, HOST_SUBCLASS_PROC, NativeHostEvents, release_subclass_reference,
};
use crate::icon;
use crate::tabbar::{
    COLOR_BORDER_ACTIVE, COLOR_BORDER_INACTIVE, Hit, TAB_BAR_HEIGHT_LOGICAL, TAB_BORDER_WIDTH,
    TabBar, TabModel,
};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey, VK_A, VK_CONTROL, VK_D,
    VK_LBUTTON, VK_T, VK_TAB, VK_W,
};
use windows::Win32::UI::Shell::{RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, HWND_TOP, MB_ICONERROR, MessageBoxW, PostMessageW, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, WM_CLOSE,
};
use windows::core::{HSTRING, PCWSTR};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

pub(crate) const SWITCH_HOTKEY_ID: i32 = 1;
pub(crate) const NEW_TAB_HOTKEY_ID: i32 = 2;
pub(crate) const CLOSE_TAB_HOTKEY_ID: i32 = 3;
pub(crate) const ATTACH_HOTKEY_ID: i32 = 4;
pub(crate) const DETACH_HOTKEY_ID: i32 = 5;
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(250);
const STARTUP_TAB_COUNT: usize = 2;
const KEY_PRESSED: u16 = 0x8000;
const TAB_DRAG_THRESHOLD_LOGICAL: f64 = 6.0;

struct CycleSession {
    order: Vec<usize>,
    step: usize,
}

#[derive(Clone, Copy)]
struct TabPress {
    guest_index: usize,
    x: i32,
    y: i32,
}

fn show_error_box(message: &str) {
    let text = HSTRING::from(message);
    let caption = HSTRING::from("Uvez");

    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_ICONERROR,
        );
    }
}

fn apply_dwm_rounding(hwnd: HWND) {
    let preference = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

fn apply_dwm_border_color(hwnd: HWND, focused: bool) {
    let rgb = if focused {
        COLOR_BORDER_ACTIVE
    } else {
        COLOR_BORDER_INACTIVE
    };
    let color = COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF));
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const COLORREF as *const c_void,
            size_of::<COLORREF>() as u32,
        );
    }
}

fn tab_sequence_for_order(display_order: &[usize], open: &[usize], total: usize) -> Vec<usize> {
    let mut sequence = Vec::new();

    for &index in display_order {
        if open.contains(&index) && !sequence.contains(&index) {
            sequence.push(index);
        }
    }

    for &index in open {
        if !sequence.contains(&index) {
            sequence.push(index);
        }
    }

    for index in 0..total {
        if !sequence.contains(&index) {
            sequence.push(index);
        }
    }

    sequence
}

fn index_remap_after_compact(total: usize, keep: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut new_index_of_old = vec![usize::MAX; total];
    let mut next_new = 0usize;

    for (old, slot) in new_index_of_old.iter_mut().enumerate() {
        if keep(old) {
            *slot = next_new;
            next_new += 1;
        }
    }

    new_index_of_old
}

pub(crate) struct App {
    window: Option<Arc<Window>>,
    managed_windows: Vec<ManagedWindow>,
    active: Option<usize>,
    mru: Vec<usize>,
    cycle: Option<CycleSession>,
    bounds_dirty: bool,
    refocus_pending: bool,
    fatal_error: Option<String>,
    switch_requested: Arc<AtomicU32>,
    new_tab_requested: Arc<AtomicU32>,
    close_tab_requested: Arc<AtomicU32>,
    attach_requested: Arc<AtomicU32>,
    attach_target: Arc<AtomicIsize>,
    detach_requested: Arc<AtomicU32>,
    native_host_events: Arc<NativeHostEvents>,
    host_subclass_reference: Option<usize>,
    lift_release_attempts: u8,
    hotkey_switch_registered: bool,
    hotkey_new_tab_registered: bool,
    hotkey_close_tab_registered: bool,
    hotkey_detach_registered: bool,
    hotkey_attach_registered: bool,
    arrival_tx: mpsc::Sender<guest::ManagedWindowArrival>,
    arrival_rx: mpsc::Receiver<guest::ManagedWindowArrival>,
    startup_spawns_pending: usize,
    tab_bar: Option<TabBar>,
    cursor_pos: Option<(i32, i32)>,
    last_strip_click: Option<(Instant, i32, i32)>,
    tab_press: Option<TabPress>,
    dwm_border_focused: Option<bool>,
    last_housekeeping: Instant,
}

impl App {
    pub(crate) fn new(
        switch_requested: Arc<AtomicU32>,
        new_tab_requested: Arc<AtomicU32>,
        close_tab_requested: Arc<AtomicU32>,
        attach_requested: Arc<AtomicU32>,
        attach_target: Arc<AtomicIsize>,
        detach_requested: Arc<AtomicU32>,
    ) -> Self {
        let (arrival_tx, arrival_rx) = mpsc::channel();

        Self {
            window: None,
            managed_windows: Vec::new(),
            active: None,
            mru: Vec::new(),
            cycle: None,
            bounds_dirty: false,
            refocus_pending: false,
            fatal_error: None,
            switch_requested,
            new_tab_requested,
            close_tab_requested,
            attach_requested,
            attach_target,
            detach_requested,
            native_host_events: Arc::new(NativeHostEvents::default()),
            host_subclass_reference: None,
            lift_release_attempts: 0,
            hotkey_switch_registered: false,
            hotkey_new_tab_registered: false,
            hotkey_close_tab_registered: false,
            hotkey_detach_registered: false,
            hotkey_attach_registered: false,
            arrival_tx,
            arrival_rx,
            startup_spawns_pending: 0,
            tab_bar: None,
            cursor_pos: None,
            last_strip_click: None,
            tab_press: None,
            dwm_border_focused: None,
            last_housekeeping: Instant::now(),
        }
    }

    fn touch_mru(&mut self, index: usize) {
        if let Some(position) = self.mru.iter().position(|existing| *existing == index) {
            let _ = self.mru.remove(position);
        }
        self.mru.insert(0, index);
    }

    fn mark_dirty(&mut self) {
        if let Some(tab_bar) = self.tab_bar.as_mut() {
            tab_bar.mark_dirty();
        }
    }

    fn scale_factor(&self) -> f64 {
        self.window
            .as_deref()
            .map(Window::scale_factor)
            .unwrap_or(1.0)
    }

    fn tab_strip_height(&self) -> i32 {
        (TAB_BAR_HEIGHT_LOGICAL * self.scale_factor()).round() as i32
    }

    fn add_managed_window(&mut self, managed: ManagedWindow) -> Result<usize, String> {
        let owner = self
            .host_hwnd()
            .ok_or_else(|| "Uvez host window is unavailable".to_string())?;
        let index = self.managed_windows.len();
        self.managed_windows.push(managed);

        if let Err(error) = self.managed_windows[index].enter_managed_mode(owner) {
            self.managed_windows.remove(index);
            return Err(error);
        }

        let managed = &self.managed_windows[index];
        debug_log!(
            "Managing {} (PID {}, HWND {:?}): {}",
            managed.exe_name,
            managed.pid,
            managed.hwnd,
            managed.title
        );
        self.mark_dirty();
        Ok(index)
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
            debug_log!("Could not remove the Uvez host window subclass safely");
        }
    }

    fn host_content_bounds(&self) -> Option<WindowBounds> {
        let window = self.window.as_ref()?;
        let position = window.inner_position().ok()?;
        let size = window.inner_size();
        let strip_height = self.tab_strip_height();
        let border = TAB_BORDER_WIDTH;
        let raw_width = i32::try_from(size.width).ok()?;
        let width = raw_width - border * 2;
        let height = size.height as i32 - strip_height - border;

        if width <= 0 || height <= 0 {
            return None;
        }

        Some(WindowBounds {
            x: position.x + border,
            y: position.y + strip_height,
            width,
            height,
        })
    }

    fn host_hwnd(&self) -> Option<HWND> {
        let handle = self.window.as_ref()?.window_handle().ok()?;

        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut c_void)),
            _ => None,
        }
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
        if host_events::release_lifted_guest(&self.native_host_events) {
            self.lift_release_attempts = 0;
            return true;
        }

        self.lift_release_attempts = self.lift_release_attempts.saturating_add(1);
        if self.lift_release_attempts == 1 {
            debug_log!("Could not restore the managed window's normal Z-order; retrying");
        }

        if self.lift_release_attempts <= 3 {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        } else if self.lift_release_attempts == 4 {
            debug_log!("Managed window Z-order restoration still failed after three retries");
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
            debug_log!(
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
            debug_log!("Could not raise the Uvez host: {error}");
        }
    }

    fn position_active_window(&self) -> bool {
        let (Some(active), Some(bounds)) = (self.active, self.host_content_bounds()) else {
            return false;
        };

        let managed = &self.managed_windows[active];
        if !managed.is_open() {
            debug_log!("Managed HWND {:?} is no longer valid", managed.hwnd);
            return false;
        }

        if let Err(error) = managed.position(bounds) {
            debug_log!("Could not position {}: {error}", managed.title);
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

        self.cycle = None;
        self.touch_mru(index);
        self.activate_window_core(index);
    }

    fn activate_window_core(&mut self, index: usize) {
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
            debug_log!(
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
        self.mark_dirty();
        self.update_hotkey_registration();
        self.sync_tab_bar_focus();
    }

    fn next_candidate(&self) -> Option<usize> {
        if self.managed_windows.is_empty() {
            return None;
        }

        let mut candidates: Vec<usize> = self
            .mru
            .iter()
            .copied()
            .filter(|index| {
                *index < self.managed_windows.len() && self.managed_windows[*index].is_open()
            })
            .collect();

        for (index, managed) in self.managed_windows.iter().enumerate() {
            if managed.is_open() && !candidates.contains(&index) {
                candidates.push(index);
            }
        }

        if let Some(&active) = self.active.as_ref()
            && let Some(position) = candidates.iter().position(|index| *index == active)
        {
            let _ = candidates.remove(position);
        }

        candidates.into_iter().next()
    }

    fn activate_next_window(&mut self) {
        if let Some(next) = self.next_candidate() {
            self.activate_window(next);
        }
    }

    fn show_next_window_without_focus(&mut self) {
        let Some(next) = self.next_candidate() else {
            self.refocus_pending = false;
            return;
        };

        for other in 0..self.managed_windows.len() {
            if other != next {
                self.hide_window(other);
            }
        }

        self.active = Some(next);
        self.touch_mru(next);
        self.native_host_events
            .active_pid
            .store(self.managed_windows[next].pid, Ordering::Release);
        self.native_host_events.active_hwnd.store(
            self.managed_windows[next].hwnd.0 as isize,
            Ordering::Release,
        );
        self.bounds_dirty = false;
        if !self.position_active_window() {
            self.managed_windows[next].hide();
        }

        self.refocus_pending = false;
        self.update_host_title();
        self.mark_dirty();
        self.update_hotkey_registration();
        self.sync_tab_bar_focus();
    }

    fn ctrl_held() -> bool {
        let state = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) };
        (state as u16 & KEY_PRESSED) != 0
    }

    fn left_button_held() -> bool {
        let state = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) };
        (state as u16 & KEY_PRESSED) != 0
    }

    fn begin_or_advance_tab_cycle(&mut self) {
        if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            self.cancel_tab_drag();
        }

        if self.cycle.is_none() {
            self.start_tab_cycle();
        } else if let Some(session) = self.cycle.as_mut() {
            session.step += 1;
        }

        self.activate_cycle_target();
    }

    fn start_tab_cycle(&mut self) {
        let mut candidates: Vec<usize> = self
            .mru
            .iter()
            .copied()
            .filter(|index| {
                *index < self.managed_windows.len() && self.managed_windows[*index].is_open()
            })
            .collect();

        for (index, managed) in self.managed_windows.iter().enumerate() {
            if managed.is_open() && !candidates.contains(&index) {
                candidates.push(index);
            }
        }

        if candidates.len() < 2 {
            return;
        }

        let active_rank = self
            .active
            .and_then(|active| candidates.iter().position(|index| *index == active))
            .unwrap_or(0);
        let order: Vec<usize> = candidates
            .iter()
            .skip(active_rank + 1)
            .chain(candidates.iter().take(active_rank + 1))
            .copied()
            .collect();

        self.cycle = Some(CycleSession { order, step: 0 });
    }

    fn activate_cycle_target(&mut self) {
        let resolved = {
            let Some(session) = self.cycle.as_mut() else {
                return;
            };

            session.order.retain(|&index| {
                index < self.managed_windows.len() && self.managed_windows[index].is_open()
            });

            (!session.order.is_empty()).then(|| session.order[session.step % session.order.len()])
        };

        match resolved {
            Some(target) => self.activate_window_core(target),
            None => self.cycle = None,
        }
    }

    fn commit_tab_cycle(&mut self) {
        if self.cycle.take().is_some()
            && let Some(active) = self.active
            && active < self.managed_windows.len()
            && self.managed_windows[active].is_open()
        {
            self.touch_mru(active);
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
            managed.close();
        }
        self.mark_dirty();
    }

    fn close_active_window(&mut self) {
        if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            self.cancel_tab_drag();
        }

        let Some(active) = self.active else {
            return;
        };
        if !self
            .managed_windows
            .get(active)
            .is_some_and(ManagedWindow::is_open)
        {
            return;
        }

        self.close_managed_window(active);
        self.activate_next_window();
    }

    fn detach_active_window(&mut self) {
        let Some(active) = self.active else {
            return;
        };
        if !self
            .managed_windows
            .get(active)
            .is_some_and(ManagedWindow::is_open)
        {
            return;
        }

        self.detach_window(active);
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
                debug_log!("Could not release {}: {error}", managed.title);
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
                debug_log!("Could not restore {} while exiting: {error}", managed.title);
            }
        }
    }

    fn adopt_arrival(
        &mut self,
        arrival: guest::ManagedWindowArrival,
        event_loop: &ActiveEventLoop,
    ) {
        let startup = self.startup_spawns_pending > 0;
        if startup {
            self.startup_spawns_pending -= 1;
        }

        match arrival {
            Ok(discovered) => {
                let index = match self.add_managed_window(discovered.into_managed_window()) {
                    Ok(index) => index,
                    Err(error) => {
                        debug_log!("Could not manage new window: {error}");
                        if startup && self.active.is_none() {
                            self.fail_startup(format!(
                                "Uvez could not manage the new window: {error}"
                            ));
                            self.reveal_managed_windows();
                            event_loop.exit();
                        }
                        return;
                    }
                };

                if !startup || self.active.is_none() {
                    self.activate_window(index);
                }
            }
            Err(error) => {
                debug_log!("Could not create managed window: {error}");
                if startup && self.active.is_none() {
                    self.fail_startup(format!(
                        "Could not start the guest application: {error}\n\nMake sure Alacritty is installed and available on PATH, then start Uvez again."
                    ));
                    self.reveal_managed_windows();
                    event_loop.exit();
                }
            }
        }
    }

    fn fail_startup(&mut self, message: String) {
        if self.fatal_error.is_none() {
            show_error_box(&message);
            self.fatal_error = Some(message);
        }
    }

    pub(crate) fn take_fatal_error(&mut self) -> Option<String> {
        self.fatal_error.take()
    }

    fn spawn_new_tab(&mut self) {
        let host_hwnd = self.host_hwnd().map(|hwnd| hwnd.0 as isize);
        guest::request_managed_window("alacritty.exe", &[], self.arrival_tx.clone(), host_hwnd);
    }

    fn register_hotkeys(&mut self) {
        if !self.hotkey_switch_registered {
            match unsafe {
                RegisterHotKey(
                    None,
                    SWITCH_HOTKEY_ID,
                    MOD_CONTROL | MOD_NOREPEAT,
                    u32::from(VK_TAB.0),
                )
            } {
                Ok(()) => self.hotkey_switch_registered = true,
                Err(error) => debug_log!("Could not register Ctrl+Tab: {error}"),
            }
        }

        if !self.hotkey_new_tab_registered {
            match unsafe {
                RegisterHotKey(
                    None,
                    NEW_TAB_HOTKEY_ID,
                    MOD_CONTROL | MOD_NOREPEAT,
                    u32::from(VK_T.0),
                )
            } {
                Ok(()) => self.hotkey_new_tab_registered = true,
                Err(error) => debug_log!("Could not register Ctrl+T: {error}"),
            }
        }

        if !self.hotkey_close_tab_registered {
            match unsafe {
                RegisterHotKey(
                    None,
                    CLOSE_TAB_HOTKEY_ID,
                    MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
                    u32::from(VK_W.0),
                )
            } {
                Ok(()) => self.hotkey_close_tab_registered = true,
                Err(error) => debug_log!("Could not register Ctrl + W: {error}"),
            }
        }

        if !self.hotkey_detach_registered {
            match unsafe {
                RegisterHotKey(
                    None,
                    DETACH_HOTKEY_ID,
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    u32::from(VK_D.0),
                )
            } {
                Ok(()) => self.hotkey_detach_registered = true,
                Err(error) => debug_log!("Could not register Ctrl+Alt+D: {error}"),
            }
        }
    }

    fn register_attach_hotkey(&mut self) {
        if self.hotkey_attach_registered {
            return;
        }

        match unsafe {
            RegisterHotKey(
                None,
                ATTACH_HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                u32::from(VK_A.0),
            )
        } {
            Ok(()) => self.hotkey_attach_registered = true,
            Err(error) => {
                debug_log!("Could not register Ctrl+Alt+A: {error}");
                debug_log!("Attaching the focused window is unavailable this session");
            }
        }
    }

    fn unregister_hotkeys(&mut self) {
        if self.hotkey_switch_registered
            && unsafe { UnregisterHotKey(None, SWITCH_HOTKEY_ID) }.is_ok()
        {
            self.hotkey_switch_registered = false;
        }
        if self.hotkey_new_tab_registered
            && unsafe { UnregisterHotKey(None, NEW_TAB_HOTKEY_ID) }.is_ok()
        {
            self.hotkey_new_tab_registered = false;
        }
        if self.hotkey_close_tab_registered
            && unsafe { UnregisterHotKey(None, CLOSE_TAB_HOTKEY_ID) }.is_ok()
        {
            self.hotkey_close_tab_registered = false;
        }
        if self.hotkey_detach_registered
            && unsafe { UnregisterHotKey(None, DETACH_HOTKEY_ID) }.is_ok()
        {
            self.hotkey_detach_registered = false;
        }
    }

    fn update_hotkey_registration(&mut self) {
        if self.group_is_foreground() {
            self.register_hotkeys();
        } else {
            self.unregister_hotkeys();
        }
    }

    fn sync_tab_bar_focus(&mut self) {
        let focused = self.group_is_foreground();
        if let Some(tab_bar) = self.tab_bar.as_mut() {
            tab_bar.set_focused(focused);
        }

        if self.dwm_border_focused != Some(focused)
            && let Some(hwnd) = self.host_hwnd()
        {
            self.dwm_border_focused = Some(focused);
            apply_dwm_border_color(hwnd, focused);
        }
    }

    fn draw_tab_bar(&mut self) {
        let size = self.window.as_deref().map(Window::inner_size);
        let Some(size) = size else { return };
        if size.width == 0 || size.height == 0 {
            return;
        }

        let models: Vec<TabModel> = self
            .managed_windows
            .iter()
            .enumerate()
            .filter(|(_, managed)| managed.is_open())
            .map(|(index, managed)| TabModel {
                guest_index: index,
                title: managed.title.clone(),
                active: self.active == Some(index),
            })
            .collect();

        if let (Some(window), Some(tab_bar)) = (&self.window, self.tab_bar.as_mut()) {
            tab_bar.draw(window, &models);
        }
    }

    fn update_hover(&mut self) {
        if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            return;
        }

        let Some((x, y)) = self.cursor_pos else {
            return;
        };

        let hit = self
            .tab_bar
            .as_ref()
            .map(|tab_bar| tab_bar.hit_test(x, y))
            .unwrap_or(Hit::None);

        if let Some(tab_bar) = self.tab_bar.as_mut() {
            tab_bar.set_hover(hit);
        }
    }

    fn toggle_host_maximized(&mut self) {
        if let Some(window) = self.window.as_deref() {
            let maximized = window.is_maximized();
            window.set_maximized(!maximized);
        }
    }

    fn begin_host_drag(&mut self) {
        if let Some(window) = self.window.as_deref() {
            let _ = window.drag_window();
        }
    }

    fn handle_strip_click(&mut self, button: MouseButton) {
        if self.native_host_events.in_size_move.load(Ordering::Acquire) {
            return;
        }

        if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            return;
        }

        self.tab_press = None;

        let Some((x, y)) = self.cursor_pos else {
            return;
        };

        let hit = self
            .tab_bar
            .as_ref()
            .map(|tab_bar| tab_bar.hit_test(x, y))
            .unwrap_or(Hit::None);
        debug_log!("Strip click {button:?} at ({x}, {y}): {hit:?}");

        match (button, hit) {
            (MouseButton::Left, Hit::NewTab) => {
                self.spawn_new_tab();
            }
            (MouseButton::Left, Hit::Tab(guest_index)) => {
                if !self
                    .managed_windows
                    .get(guest_index)
                    .is_some_and(ManagedWindow::is_open)
                {
                    return;
                }

                if self.active == Some(guest_index) {
                    self.reactivate_active_window();
                    self.refocus_pending = false;
                } else {
                    self.activate_window(guest_index);
                }
                self.tab_press = Some(TabPress { guest_index, x, y });
            }
            (MouseButton::Left, Hit::Close(guest_index))
            | (MouseButton::Middle, Hit::Tab(guest_index)) => {
                if !self
                    .managed_windows
                    .get(guest_index)
                    .is_some_and(ManagedWindow::is_open)
                {
                    return;
                }

                let was_active = self.active == Some(guest_index);
                self.close_managed_window(guest_index);
                if was_active {
                    self.activate_next_window();
                }
            }
            (MouseButton::Left, Hit::Detach(guest_index)) => {
                if !self
                    .managed_windows
                    .get(guest_index)
                    .is_some_and(ManagedWindow::is_open)
                {
                    return;
                }

                self.detach_window(guest_index);
            }
            (MouseButton::Left, Hit::Minimize) => {
                if let Some(window) = self.window.as_deref() {
                    window.set_minimized(true);
                }
            }
            (MouseButton::Left, Hit::Maximize) => self.toggle_host_maximized(),
            (MouseButton::Left, Hit::CloseWindow) => {
                if let Some(hwnd) = self.host_hwnd() {
                    unsafe {
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            (MouseButton::Left, Hit::None) => {
                if y >= self.tab_strip_height() {
                    return;
                }

                let double_click_time = unsafe { GetDoubleClickTime() } as u128;
                let snap = (4.0 * self.scale_factor()) as i32;
                if let Some((when, last_x, last_y)) = self.last_strip_click
                    && when.elapsed().as_millis() <= double_click_time
                    && (last_x - x).abs() <= snap
                    && (last_y - y).abs() <= snap
                {
                    self.last_strip_click = None;
                    self.toggle_host_maximized();
                    return;
                }

                self.last_strip_click = Some((Instant::now(), x, y));
                self.begin_host_drag();
            }
            _ => {}
        }
    }

    fn promote_tab_press(&mut self, x: i32, y: i32) {
        let Some(press) = self.tab_press else {
            return;
        };

        let threshold = (TAB_DRAG_THRESHOLD_LOGICAL * self.scale_factor()).round() as i32;
        let threshold = threshold.max(1);
        if (x - press.x).abs() < threshold && (y - press.y).abs() < threshold {
            return;
        }

        self.tab_press = None;
        if !Self::left_button_held() {
            return;
        }

        let host_hwnd = self.host_hwnd();
        let Some(tab_bar) = self.tab_bar.as_mut() else {
            return;
        };
        if tab_bar.begin_drag(press.guest_index, press.x)
            && let Some(hwnd) = host_hwnd
        {
            unsafe {
                let _ = SetCapture(hwnd);
            }
            debug_log!("Dragging tab {}", press.guest_index + 1);
        }
    }

    fn handle_strip_release(&mut self, button: MouseButton) {
        self.tab_press = None;
        if button != MouseButton::Left {
            return;
        }
        if !self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            return;
        }
        self.finish_tab_drag();
    }

    fn finish_tab_drag(&mut self) {
        let cursor_x = self.cursor_pos.map(|(x, _)| x);
        let order = self
            .tab_bar
            .as_mut()
            .and_then(|tab_bar| tab_bar.finish_drag(cursor_x));
        unsafe {
            let _ = ReleaseCapture();
        }
        if let Some(order) = order {
            self.apply_tab_order(order);
        }
        self.update_hover();
    }

    fn cancel_tab_drag(&mut self) {
        self.tab_press = None;
        if let Some(tab_bar) = self.tab_bar.as_mut() {
            tab_bar.cancel_drag();
        }
        unsafe {
            let _ = ReleaseCapture();
        }
        self.update_hover();
    }

    fn apply_tab_order(&mut self, display_order: Vec<usize>) {
        let original_len = self.managed_windows.len();
        if original_len < 2 {
            return;
        }

        let open: Vec<usize> = (0..original_len)
            .filter(|&index| self.managed_windows[index].is_open())
            .collect();
        let sequence = tab_sequence_for_order(&display_order, &open, original_len);

        let unchanged = sequence
            .iter()
            .enumerate()
            .all(|(position, &index)| position == index);
        if unchanged {
            return;
        }

        let mut new_index_of_old = vec![usize::MAX; original_len];
        for (position, &index) in sequence.iter().enumerate() {
            new_index_of_old[index] = position;
        }

        let old_windows = std::mem::take(&mut self.managed_windows);
        let mut slots: Vec<Option<ManagedWindow>> = old_windows.into_iter().map(Some).collect();
        self.managed_windows = sequence
            .iter()
            .map(|&index| slots[index].take().expect("tab sequence is a permutation"))
            .collect();

        if let Some(active) = self.active {
            self.active = Some(new_index_of_old[active]);
        }

        self.mru.retain(|&index| index < original_len);
        self.mru = self
            .mru
            .iter()
            .map(|&index| new_index_of_old[index])
            .collect();

        if let Some(session) = &mut self.cycle {
            session.order.retain(|&index| index < original_len);
            session.order = session
                .order
                .iter()
                .map(|&index| new_index_of_old[index])
                .collect();
        }

        debug_log!("Reordered tabs: {sequence:?}");
        self.mark_dirty();
    }

    fn attach_window(&mut self, target_value: isize) {
        if target_value == 0 {
            return;
        }

        let target = HWND(target_value as *mut c_void);
        if self.host_hwnd().is_some_and(|host| host == target) {
            return;
        }

        if self
            .managed_windows
            .iter()
            .any(|managed| managed.is_open() && managed.hwnd == target)
        {
            return;
        }

        if guest::get_window_process_id(target) == std::process::id() {
            return;
        }

        debug_log!("Attach requested for HWND {target:?}");
        let host_hwnd = self.host_hwnd().map(|hwnd| hwnd.0 as isize);
        guest::request_adopt_window(target_value, self.arrival_tx.clone(), host_hwnd);
    }

    fn detach_window(&mut self, index: usize) {
        if !self
            .managed_windows
            .get(index)
            .is_some_and(ManagedWindow::is_open)
        {
            return;
        }

        if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
            self.cancel_tab_drag();
        }

        let was_active = self.active == Some(index);
        if was_active {
            let _ = self.reconcile_move_lift();
        }

        if let Err(error) = self.managed_windows[index].restore_native_state() {
            debug_log!(
                "Could not detach {}: {error}",
                self.managed_windows[index].title
            );
            return;
        }

        let title = self.managed_windows[index].title.clone();
        self.managed_windows[index].activate();
        debug_log!("Detached {title} into a standalone window");

        self.compact_managed_windows(|existing, _| existing != index);

        if was_active {
            self.show_next_window_without_focus();
        }

        self.update_host_title();
        self.update_hotkey_registration();
        self.sync_tab_bar_focus();
        self.mark_dirty();
    }

    fn compact_managed_windows(&mut self, keep: impl Fn(usize, &ManagedWindow) -> bool) -> bool {
        let original_len = self.managed_windows.len();
        if original_len == 0 {
            return false;
        }

        let new_index_of_old = index_remap_after_compact(original_len, |index| {
            keep(index, &self.managed_windows[index])
        });

        let mut old_index = 0usize;
        self.managed_windows.retain(|managed| {
            let retained = keep(old_index, managed);
            old_index += 1;
            retained
        });

        if new_index_of_old.iter().all(|&mapped| mapped != usize::MAX) {
            return false;
        }

        if let Some(active) = self.active {
            if new_index_of_old[active] != usize::MAX {
                self.active = Some(new_index_of_old[active]);
            } else {
                self.active = None;
                self.native_host_events
                    .active_hwnd
                    .store(0, Ordering::Release);
                self.native_host_events
                    .active_pid
                    .store(0, Ordering::Release);
            }
        }

        self.mru = self
            .mru
            .iter()
            .filter_map(|&index| new_index_of_old.get(index).copied())
            .filter(|&mapped| mapped != usize::MAX)
            .collect();

        if let Some(session) = &mut self.cycle {
            session.order = session
                .order
                .iter()
                .filter_map(|&index| {
                    let mapped = new_index_of_old.get(index).copied()?;
                    (mapped != usize::MAX
                        && self
                            .managed_windows
                            .get(mapped)
                            .is_some_and(ManagedWindow::is_open))
                    .then_some(mapped)
                })
                .collect();
        }

        self.mark_dirty();
        true
    }

    fn prune_dead_guests(&mut self) -> bool {
        let active_died = self
            .active
            .is_some_and(|active| !self.managed_windows[active].is_open());

        let changed = self.compact_managed_windows(|_index, managed| managed.is_open());
        if changed && active_died {
            self.activate_next_window();
        }

        changed
    }

    fn housekeeping(&mut self) {
        self.last_housekeeping = Instant::now();

        let mut titles_changed = false;
        for managed in &mut self.managed_windows {
            if !managed.is_open() {
                continue;
            }

            let raw_title = guest::get_window_title(managed.hwnd);
            let title = guest::format_title(&raw_title, &managed.exe_name);
            if !title.is_empty() && title != managed.title {
                managed.title = title;
                titles_changed = true;
            }
        }

        if titles_changed {
            if self.active.is_some() {
                self.update_host_title();
            }
            self.mark_dirty();
        }

        let active_died = self
            .active
            .is_some_and(|active| !self.managed_windows[active].is_open());

        if self.prune_dead_guests() && active_died {
            self.activate_next_window();
        }

        self.update_hotkey_registration();
        self.sync_tab_bar_focus();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attributes = Window::default_attributes()
            .with_title("Uvez")
            .with_window_icon(icon::window_icon());
        attributes = attributes.with_taskbar_icon(icon::taskbar_icon());
        self.window = Some(Arc::new(event_loop.create_window(attributes).unwrap()));

        if let Err(error) = self.install_host_subclass() {
            debug_log!("Could not initialize native host synchronization: {error}");
            self.fail_startup(format!(
                "Uvez could not initialize native host synchronization: {error}"
            ));
            event_loop.exit();
            return;
        }

        if let Some(hwnd) = self.host_hwnd() {
            apply_dwm_rounding(hwnd);
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }

        match TabBar::new(self.window.as_ref().expect("host window exists")) {
            Ok(tab_bar) => self.tab_bar = Some(tab_bar),
            Err(error) => {
                debug_log!("Could not initialize the tab bar renderer: {error}");
                self.fail_startup(format!(
                    "Uvez could not initialize the tab bar renderer: {error}"
                ));
                self.reveal_managed_windows();
                event_loop.exit();
                return;
            }
        }

        for _ in 0..STARTUP_TAB_COUNT {
            self.spawn_new_tab();
        }

        self.register_attach_hotkey();

        debug_log!(
            "Hotkeys active while Uvez is focused: Ctrl+Tab switches tabs, Ctrl+T opens a new tab, Ctrl+Shift+W closes the active tab, Ctrl+Alt+D detaches the active tab"
        );
        debug_log!("Ctrl+Alt+A attaches the currently focused window as a new tab");
        self.update_hotkey_registration();
        self.sync_tab_bar_focus();
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
                self.cycle = None;
                self.native_host_events
                    .active_hwnd
                    .store(0, Ordering::Release);
                self.native_host_events
                    .active_pid
                    .store(0, Ordering::Release);
                self.active = None;
                self.bounds_dirty = false;
                self.refocus_pending = false;
                self.switch_requested.store(0, Ordering::Release);
                self.new_tab_requested.store(0, Ordering::Release);
                self.close_tab_requested.store(0, Ordering::Release);
                self.attach_requested.store(0, Ordering::Release);
                self.attach_target.store(0, Ordering::Release);
                self.detach_requested.store(0, Ordering::Release);
                self.close_all_managed_windows();
                event_loop.exit();
            }

            WindowEvent::RedrawRequested => {
                self.draw_tab_bar();
            }

            WindowEvent::Focused(true) => {
                self.raise_active_without_activation();
                self.sync_group_bounds();
                self.refocus_pending = true;
                self.update_hotkey_registration();
                self.sync_tab_bar_focus();
            }

            WindowEvent::Focused(false) => {
                let _ = self.reconcile_move_lift();
                if !self.group_is_foreground() {
                    self.refocus_pending = false;
                    if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging)
                        || self.tab_press.is_some()
                    {
                        self.cancel_tab_drag();
                    }
                }
                self.update_hotkey_registration();
                self.sync_tab_bar_focus();
            }

            WindowEvent::Moved(_) => {
                let group_is_foreground = self.group_is_foreground();
                self.sync_group_bounds();
                self.refocus_pending = group_is_foreground;
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.cancel_tab_drag();
                if let Some(tab_bar) = self.tab_bar.as_mut() {
                    tab_bar.update_scale(scale_factor);
                }
                let group_is_foreground = self.group_is_foreground();
                self.sync_group_bounds();
                self.refocus_pending = group_is_foreground;
            }

            WindowEvent::Resized(size) => {
                self.mark_dirty();
                let collapse_threshold = self.tab_strip_height();
                if size.width == 0 || (size.height as i32) <= collapse_threshold {
                    if let Some(active) = self.active {
                        self.hide_window(active);
                    }
                    self.bounds_dirty = false;
                    self.refocus_pending = false;
                } else {
                    let group_is_foreground = self.group_is_foreground();
                    self.sync_group_bounds();
                    self.refocus_pending = group_is_foreground;
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let x = position.x as i32;
                let y = position.y as i32;
                self.cursor_pos = Some((x, y));
                self.promote_tab_press(x, y);

                if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
                    if let Some(tab_bar) = self.tab_bar.as_mut()
                        && tab_bar.update_drag(x)
                    {
                        self.mark_dirty();
                    }
                } else {
                    self.update_hover();
                }
            }

            WindowEvent::CursorLeft { .. } => {
                if self.tab_bar.as_ref().is_some_and(TabBar::is_dragging) {
                    return;
                }

                self.cursor_pos = None;
                if let Some(tab_bar) = self.tab_bar.as_mut() {
                    tab_bar.clear_hover();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let Some((_, y)) = self.cursor_pos else {
                    return;
                };
                if y >= self.tab_strip_height()
                    || self.tab_bar.as_ref().is_some_and(TabBar::is_dragging)
                {
                    return;
                }

                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(position) => {
                        (position.y / (48.0 * self.scale_factor())) as f32
                    }
                };

                if let Some(tab_bar) = self.tab_bar.as_mut() {
                    tab_bar.scroll_by_wheel(lines);
                }
            }

            WindowEvent::MouseInput { state, button, .. } => match state {
                ElementState::Pressed => self.handle_strip_click(button),
                ElementState::Released => self.handle_strip_release(button),
            },

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.cycle.is_some() && !Self::ctrl_held() {
            self.commit_tab_cycle();
        }
        event_loop.set_control_flow(if self.cycle.is_some() {
            ControlFlow::Poll
        } else {
            ControlFlow::WaitUntil(self.last_housekeeping + HOUSEKEEPING_INTERVAL)
        });

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

        while let Ok(arrival) = self.arrival_rx.try_recv() {
            self.adopt_arrival(arrival, event_loop);
        }

        while self
            .switch_requested
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            self.begin_or_advance_tab_cycle();
        }

        while self
            .new_tab_requested
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            self.spawn_new_tab();
        }

        while self
            .close_tab_requested
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            self.close_active_window();
        }

        while self
            .attach_requested
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            let target = self.attach_target.swap(0, Ordering::AcqRel);
            self.attach_window(target);
        }

        while self
            .detach_requested
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            self.detach_active_window();
        }

        if self.last_housekeeping.elapsed() >= HOUSEKEEPING_INTERVAL {
            self.housekeeping();
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

        let wants_redraw = self.tab_bar.as_mut().is_some_and(TabBar::take_dirty);
        if wants_redraw && let Some(window) = &self.window {
            window.request_redraw();
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
        if self.hotkey_attach_registered {
            unsafe {
                let _ = UnregisterHotKey(None, ATTACH_HOTKEY_ID);
            }
            self.hotkey_attach_registered = false;
        }
        self.unregister_hotkeys();
        self.remove_host_subclass();
    }
}

#[cfg(test)]
mod tests {
    use super::{index_remap_after_compact, tab_sequence_for_order};

    #[test]
    fn identity_order_passes_through() {
        let open = [0, 1, 2];
        assert_eq!(tab_sequence_for_order(&[0, 1, 2], &open, 3), vec![0, 1, 2]);
    }

    #[test]
    fn display_order_reorders_open_guests() {
        let open = [0, 1, 2];
        assert_eq!(tab_sequence_for_order(&[2, 0, 1], &open, 3), vec![2, 0, 1]);
    }

    #[test]
    fn partial_display_order_appends_missing_open_guests() {
        let open = [0, 1, 2];
        assert_eq!(tab_sequence_for_order(&[2], &open, 3), vec![2, 0, 1]);
    }

    #[test]
    fn unknown_and_duplicate_indices_are_dropped() {
        let open = [0, 1, 2];
        assert_eq!(
            tab_sequence_for_order(&[1, 1, 7, 0], &open, 3),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn closed_guests_keep_relative_order_after_open_ones() {
        let open = [0, 2];
        assert_eq!(tab_sequence_for_order(&[2, 0], &open, 3), vec![2, 0, 1]);
        assert_eq!(tab_sequence_for_order(&[], &open, 3), vec![0, 2, 1]);
    }

    #[test]
    fn out_of_range_indices_are_ignored() {
        let open = [0, 1];
        assert_eq!(tab_sequence_for_order(&[1, 0], &open, 2), vec![1, 0]);
    }

    #[test]
    fn remap_without_removals_is_identity() {
        assert_eq!(index_remap_after_compact(3, |_| true), vec![0, 1, 2]);
    }

    #[test]
    fn remap_after_removing_middle() {
        let remap = index_remap_after_compact(3, |index| index != 1);
        assert_eq!(remap, vec![0, usize::MAX, 1]);
    }

    #[test]
    fn remap_after_removing_first() {
        let remap = index_remap_after_compact(3, |index| index != 0);
        assert_eq!(remap, vec![usize::MAX, 0, 1]);
    }

    #[test]
    fn remap_with_nothing_kept() {
        assert_eq!(
            index_remap_after_compact(2, |_| false),
            vec![usize::MAX, usize::MAX]
        );
    }
}
