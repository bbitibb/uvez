use std::process::Command;
use std::thread;
use std::time::Duration;

use windows::Win32::UI::WindowsAndMessaging::{self, EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SWP_NOZORDER, SetWindowPos, WNDENUMPROC};
use windows::Win32::Foundation::{self, HWND, LPARAM};
use windows::core::{self, BOOL, Result};
use winit::window::Window;


pub struct WindowInfo {
    hwnd: HWND,
    title: String,
    pid: u32,
    is_visible: bool
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

fn get_window_process_id(hwnd: HWND) -> u32 {

    let mut pid: u32 = 0;

    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)); }

    pid
}

fn launch_alacritty() -> u32 {
    let child = Command::new("alacritty.exe").spawn().expect("Failed to launch Alacritty.");

    child.id()
}

fn main() -> Result<()> {
    let alacritty_pid = launch_alacritty();
    let mut alacritty_hwnd: Option<HWND> = None;

    let ws = get_all_windows()?;

//  ws.iter().for_each(|x| if x.pid == alacritty_pid {x.print()});
    let mut found = false;
    while !found {
        let ws = get_all_windows()?;

        for i in &ws {
            if i.pid == alacritty_pid {
                found = true;
                alacritty_hwnd = Some(i.hwnd);
                i.print();
                break;
            }
        }

        if !found {
            thread::sleep(Duration::from_millis(100));
        }
    }

    if let Some(hwnd) = alacritty_hwnd {
        unsafe {
            SetWindowPos(hwnd, None, 400, 100, 800, 600, SWP_NOZORDER)?;
        }
    }

    Ok(())
}