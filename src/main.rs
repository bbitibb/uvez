use std::ffi::c_void;
use std::process::Command;
use std::{fmt, thread};
use std::time::Duration;

use windows::Win32::UI::WindowsAndMessaging::{self, EnumWindows, GWL_STYLE, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SWP_FRAMECHANGED, SWP_NOZORDER, SetParent, SetWindowLongPtrW, SetWindowPos, WNDENUMPROC, WS_CAPTION, WS_CHILD, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME};
use windows::Win32::Foundation::{self, HWND, LPARAM};
use windows::core::{self, BOOL, Result};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use windows::Win32::System::Threading::{
    AttachThreadInput,
    GetCurrentThreadId,
};

use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

#[derive(Clone, Default)]
pub struct WindowInfo {
    hwnd: HWND,
    title: String,
    pid: u32,
    is_visible: bool
}

impl fmt::Display for WindowInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} | {} | {} ",
            self.title,
            self.pid,
            self.is_visible
        )
    }
}
#[derive(Default)]
struct App {
    window: Option<Window>,
    alacritty_hwnd: Option<HWND>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attributes = Window::default_attributes().with_title("Uvez");
            let window = event_loop.create_window(attributes).unwrap();
            let handle = window.window_handle().unwrap();
            let hwnd = match handle.as_raw() {
                RawWindowHandle::Win32(h) => { HWND(h.hwnd.get() as *mut c_void) }
                _ => panic!("Not running on Windows!")
            };

            let alacritty_pid = launch_alacritty();
            let alacritty_hwnd = get_hwnd_from_pid(alacritty_pid).unwrap();

            unsafe { 
                let style = GetWindowLongPtrW(alacritty_hwnd, GWL_STYLE);
                
            let new_style =
                (style
                    & !(WS_POPUP.0 as isize)
                    & !(WS_CAPTION.0 as isize)
                    & !(WS_THICKFRAME.0 as isize)
                    & !(WS_MINIMIZEBOX.0 as isize)
                    & !(WS_MAXIMIZEBOX.0 as isize)
                    & !(WS_SYSMENU.0 as isize))
                | (WS_CHILD.0 as isize);

                SetParent(alacritty_hwnd, Some(hwnd)).unwrap();
                
                SetWindowLongPtrW(alacritty_hwnd,GWL_STYLE,new_style);

                SetWindowPos(
                    alacritty_hwnd,
                    None,
                    0,
                    0,
                    800,
                    600,
                    SWP_NOZORDER | SWP_FRAMECHANGED,
                ).unwrap();
            };
            self.alacritty_hwnd = Some(alacritty_hwnd);
            self.window = Some(window);
        }
    }

    fn window_event(&mut self,event_loop: &ActiveEventLoop,_window_id: WindowId,event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Focused(true) => {
                if let Some(hwnd) = self.alacritty_hwnd {
                    focus_window(hwnd);
                }
            }

            _ => {}
        }
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
            (&mut *vec_ptr).push(WindowInfo{hwnd, title, pid, is_visible});
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
        let alacritty_thread = GetWindowThreadProcessId(hwnd, None);
        let uvez_thread = GetCurrentThreadId();

        AttachThreadInput(uvez_thread, alacritty_thread, true.into()).unwrap();

        let _ = SetFocus(Some(hwnd));

        AttachThreadInput(uvez_thread, alacritty_thread, false.into()).unwrap();
    }
}

fn get_window_info(hwnd: HWND) -> Option<WindowInfo> {
    let all_windows = get_all_windows().unwrap();

    if let Some(w) = all_windows.iter().find(|x| x.hwnd == hwnd) {
        return Some(w.clone());
    }
    else { return None; }
}

fn get_window_process_id(hwnd: HWND) -> u32 {

    let mut pid: u32 = 0;

    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)); }

    pid
}

fn launch_alacritty() -> u32 {
    let child = Command::new("alacritty.exe").spawn().expect("Failed to launch Alacritty.");

    child.id()
}

fn get_hwnd_from_pid(pid: u32) -> Option<HWND> {
    let mut alacritty_hwnd: Option<HWND> = None;

    while alacritty_hwnd == None {
        let ws = get_all_windows().unwrap();

        for i in &ws {
            if i.pid == pid {
                alacritty_hwnd = Some(i.hwnd);
                break;
            }
        }

        if alacritty_hwnd != None {
            thread::sleep(Duration::from_millis(100));
        }
    }

    alacritty_hwnd
}

fn resize_alacritty(alacritty_hwnd: Option<HWND>) {
    if let Some(hwnd) = alacritty_hwnd {
        unsafe {
            SetWindowPos(hwnd, None, 400, 100, 800, 600, SWP_NOZORDER).unwrap();
        }
    }
}

fn main() -> Result<()> {
    //let alacritty_pid = launch_alacritty();

    //let hwnd = get_alacritty_hwnd(alacritty_pid);

    //resize_alacritty(hwnd);

    
    let event_loop = EventLoop::new().unwrap();

    let mut app = App::default();

    event_loop.run_app(&mut app).unwrap();

    Ok(())
}