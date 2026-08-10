use std::ffi::c_void;
use std::process::Command;
use std::time::Duration;
use std::{fmt, thread};

use windows::Win32::Foundation::{self, HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    self, EnumWindows, GWL_STYLE, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_NCACTIVATE, WNDENUMPROC, WS_CAPTION, WS_CHILD, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};
use windows::core::{self, BOOL, Result};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};

use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};

#[derive(Clone, Default)]
pub struct WindowInfo {
    hwnd: HWND,
    title: String,
    pid: u32,
    is_visible: bool,
}

impl fmt::Display for WindowInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} | {} | {} ", self.title, self.pid, self.is_visible)
    }
}

struct ManagedWindow {
    hwnd: HWND,
    pid: u32,
    title: String,
    exe_name: String,
}

impl ManagedWindow {
    fn focus(&self) {
        focus_window(self.hwnd);
    }

    fn resize(&self, width: i32, height: i32) {
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
        }
    }

    fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn embed(&self, parent: HWND) {
        embed_window(self.hwnd, parent);
    }
}

#[derive(Default)]
struct App {
    window: Option<Window>,
    managed_windows: Vec<ManagedWindow>,
    active: Option<usize>,
    refocus_pending: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = Window::default_attributes().with_title("Uvez");
        let window = event_loop.create_window(attributes).unwrap();

        let handle = window.window_handle().unwrap();

        let uvez_hwnd = match handle.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut c_void),
            _ => panic!("Not running on Windows!"),
        };

        let managed = create_managed_window("alacritty.exe");

        managed.embed(uvez_hwnd);

        let size = window.inner_size();

        managed.resize(size.width as i32, size.height as i32);

        self.managed_windows.push(managed);
        self.active = Some(0);
        self.window = Some(window);

        self.refocus_pending = true;
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Focused(true) => {
                self.refocus_pending = true;
            }

            WindowEvent::Resized(size) => {
                if let Some(active) = self.active {
                    self.managed_windows[active].resize(size.width as i32, size.height as i32);
                }
            }

            _ => {}
        }
    }
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if !self.refocus_pending {
            return;
        }

        if let Some(active) = self.active {
            self.managed_windows[active].focus();
        }

        self.refocus_pending = false;
    }
}

impl WindowInfo {
    pub fn print(&self) {
        let title = &self.title;
        let pid = self.pid;
        let is_visible = self.is_visible;
        println!("{title} | {pid} | {is_visible}");
    }
}

fn embed_window(child: HWND, parent: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(child, GWL_STYLE);

        let new_style = (style
            & !(WS_POPUP.0 as isize)
            & !(WS_CAPTION.0 as isize)
            & !(WS_THICKFRAME.0 as isize)
            & !(WS_MINIMIZEBOX.0 as isize)
            & !(WS_MAXIMIZEBOX.0 as isize)
            & !(WS_SYSMENU.0 as isize))
            | (WS_CHILD.0 as isize);

        SetWindowLongPtrW(child, GWL_STYLE, new_style);

        SetParent(child, Some(parent)).unwrap();

        SetWindowPos(
            child,
            None,
            0,
            0,
            800,
            600,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED ,
        )
        .unwrap();
    }
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let vec_ptr = lparam.0 as *mut Vec<WindowInfo>;

    unsafe {
        let text_len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; (text_len + 1) as usize];
        let written = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..written as usize]);

        let pid = get_window_process_id(hwnd);
        let is_visible = IsWindowVisible(hwnd).as_bool();

        if !title.is_empty() && is_visible {
            (&mut *vec_ptr).push(WindowInfo {
                hwnd,
                title,
                pid,
                is_visible,
            });
        }
    }

    BOOL(1)
}

fn get_all_windows() -> Result<Vec<WindowInfo>> {
    let mut ws: Vec<WindowInfo> = Vec::new();
    let wptr: *mut Vec<WindowInfo> = &mut ws;
    let wparam = LPARAM(wptr as isize);

    unsafe {
        EnumWindows(Some(enum_callback), wparam)?;
    }

    Ok(ws)
}

fn focus_window(hwnd: HWND) {
    unsafe {
        let child_thread = GetWindowThreadProcessId(hwnd, None);
        let uvez_thread = GetCurrentThreadId();

        AttachThreadInput(
            uvez_thread,
            child_thread,
            true.into(),
        ).unwrap();

        let _ = SetFocus(Some(hwnd));

        let _ = SendMessageW(
            hwnd,
            WM_NCACTIVATE,
            Some(WPARAM(1)),
            Some(LPARAM(0)),
        );

        AttachThreadInput(
            uvez_thread,
            child_thread,
            false.into(),
        ).unwrap();
    }
}

fn get_window_info(hwnd: HWND) -> Option<WindowInfo> {
    let all_windows = get_all_windows().unwrap();

    if let Some(w) = all_windows.iter().find(|x| x.hwnd == hwnd) {
        return Some(w.clone());
    } else {
        return None;
    }
}

fn get_window_process_id(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;

    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }

    pid
}

fn launch_process(process_name: &str) -> u32 {
    let child = Command::new(process_name)
        .arg("--print-events")
        .spawn()
        .expect("Failed to launch.");

    child.id()
}

fn create_managed_window(process_name: &str) -> ManagedWindow {
    let pid = launch_process(process_name);

    let hwnd = get_hwnd_from_pid(pid).unwrap();

    let info = get_window_info(hwnd).unwrap();

    ManagedWindow {
        hwnd,
        pid,
        title: info.title,
        exe_name: process_name.to_string(),
    }
}

fn get_hwnd_from_pid(pid: u32) -> Option<HWND> {
    let mut hwnd: Option<HWND> = None;

    while hwnd == None {
        let ws = get_all_windows().unwrap();

        for i in &ws {
            if i.pid == pid {
                hwnd = Some(i.hwnd);
                break;
            }
        }

        if hwnd.is_none() {
            thread::sleep(Duration::from_millis(100));
        }
    }

    hwnd
}

fn main() -> Result<()> {
    let event_loop = EventLoop::new().unwrap();

    let mut app = App::default();

    event_loop.run_app(&mut app).unwrap();

    Ok(())
}
