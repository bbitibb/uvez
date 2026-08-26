use std::ffi::c_void;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::guest::{self, ManagedWindow, WindowBounds, create_managed_window};
use crate::host_events::{
    self, HOST_SUBCLASS_ID, HOST_SUBCLASS_PROC, NativeHostEvents, release_subclass_reference,
};
use crate::tabbar::{Hit, TAB_BAR_HEIGHT_LOGICAL, TabBar, TabModel};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_TAB,
};
use windows::Win32::UI::Shell::{RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

pub(crate) const SWITCH_HOTKEY_ID: i32 = 1;
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct App {
    window: Option<Arc<Window>>,
    managed_windows: Vec<ManagedWindow>,
    active: Option<usize>,
    mru: Vec<usize>,
    bounds_dirty: bool,
    refocus_pending: bool,
    switch_requested: Arc<AtomicBool>,
    native_host_events: Arc<NativeHostEvents>,
    host_subclass_reference: Option<usize>,
    lift_release_attempts: u8,
    hotkey_registered: bool,
    tab_bar: Option<TabBar>,
    cursor_pos: Option<(i32, i32)>,
    last_housekeeping: Instant,
}

impl App {
    pub(crate) fn new(switch_requested: Arc<AtomicBool>) -> Self {
        Self {
            window: None,
            managed_windows: Vec::new(),
            active: None,
            mru: Vec::new(),
            bounds_dirty: false,
            refocus_pending: false,
            switch_requested,
            native_host_events: Arc::new(NativeHostEvents::default()),
            host_subclass_reference: None,
            lift_release_attempts: 0,
            hotkey_registered: false,
            tab_bar: None,
            cursor_pos: None,
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

    fn add_managed_window(&mut self, managed: ManagedWindow) -> Result<(), String> {
        let index = self.managed_windows.len();
        self.managed_windows.push(managed);

        self.managed_windows[index].enter_managed_mode()?;
        let managed = &self.managed_windows[index];
        println!(
            "Managing {} (PID {}, HWND {:?}): {}",
            managed.exe_name, managed.pid, managed.hwnd, managed.title
        );
        self.mark_dirty();
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
        let strip_height = self.tab_strip_height();
        let height = size.height as i32 - strip_height;

        if size.width == 0 || height <= 0 {
            return None;
        }

        Some(WindowBounds {
            x: position.x,
            y: position.y + strip_height,
            width: size.width.try_into().ok()?,
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
        self.touch_mru(index);
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
        self.mark_dirty();
    }

    fn activate_next_window(&mut self) {
        if self.managed_windows.is_empty() {
            return;
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

        if let Some(next) = candidates.into_iter().next() {
            self.activate_window(next);
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
                MOD_CONTROL | MOD_NOREPEAT,
                u32::from(VK_TAB.0),
            )
        };

        match result {
            Ok(()) => {
                self.hotkey_registered = true;
                println!("Press Ctrl+Tab to switch to the most recently used tab");
            }
            Err(error) => eprintln!("Could not register Ctrl+Tab: {error}"),
        }
    }

    fn unregister_switch_hotkey(&mut self) {
        if !self.hotkey_registered {
            return;
        }

        if let Err(error) = unsafe { UnregisterHotKey(None, SWITCH_HOTKEY_ID) } {
            eprintln!("Could not unregister Ctrl+Tab: {error}");
        }
        self.hotkey_registered = false;
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

    fn handle_strip_click(&mut self, button: MouseButton) {
        if self.native_host_events.in_size_move.load(Ordering::Acquire) {
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
        println!("Strip click {button:?} at ({x}, {y}): {hit:?}");

        match (button, hit) {
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
            _ => {}
        }
    }

    fn prune_dead_guests(&mut self) -> bool {
        let original_len = self.managed_windows.len();
        if original_len == 0 {
            return false;
        }

        let mut new_index_of_old = vec![usize::MAX; original_len];
        let mut next_new = 0usize;
        let mut old_index = 0usize;

        self.managed_windows.retain(|managed| {
            let keep = managed.is_open();
            if keep {
                new_index_of_old[old_index] = next_new;
                next_new += 1;
            }
            old_index += 1;
            keep
        });

        if next_new == original_len {
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

        self.mru.retain(|index| *index < self.managed_windows.len());
        self.mru = self
            .mru
            .iter()
            .map(|index| new_index_of_old[*index])
            .collect();

        self.mark_dirty();
        true
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
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = Window::default_attributes().with_title("Uvez");
        self.window = Some(Arc::new(event_loop.create_window(attributes).unwrap()));

        if let Err(error) = self.install_host_subclass() {
            eprintln!("Could not initialize native host synchronization: {error}");
            event_loop.exit();
            return;
        }

        match TabBar::new(self.window.as_ref().expect("host window exists")) {
            Ok(tab_bar) => self.tab_bar = Some(tab_bar),
            Err(error) => {
                eprintln!("Could not initialize the tab bar renderer: {error}");
                self.reveal_managed_windows();
                event_loop.exit();
                return;
            }
        }

        for _ in 0..2 {
            match create_managed_window("alacritty.exe", &[]) {
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

            WindowEvent::RedrawRequested => {
                self.draw_tab_bar();
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

            WindowEvent::Moved(_) => {
                let group_is_foreground = self.group_is_foreground();
                self.sync_group_bounds();
                self.refocus_pending = group_is_foreground;
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
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
                self.cursor_pos = Some((position.x as i32, position.y as i32));
                self.update_hover();
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = None;
                if let Some(tab_bar) = self.tab_bar.as_mut() {
                    tab_bar.clear_hover();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => self.handle_strip_click(button),

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
        self.unregister_switch_hotkey();
        self.remove_host_subclass();
    }
}
