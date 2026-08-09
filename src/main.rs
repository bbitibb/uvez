use windows::Win32::UI::WindowsAndMessaging::{self, EnumWindows, GetWindowTextLengthW, GetWindowTextW, WNDENUMPROC};
use windows::Win32::Foundation::{self, HWND, LPARAM};
use windows::core::{self, BOOL, Result};


unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let vec_ptr = lparam.0 as *mut Vec<String>;

    
    
    unsafe {
        let text_len = GetWindowTextLengthW(hwnd);
        
        let mut buf = vec![0u16; (text_len + 1) as usize];
        
        let written = GetWindowTextW(hwnd, &mut buf);

        let title = String::from_utf16_lossy(&buf[..written as usize]);

        (&mut *vec_ptr).push(title);
    }

    BOOL(1)
}

fn get_all_windows() -> Result<Vec<String>> {

    let mut ws: Vec<String> = Vec::new();
    let wptr: *mut Vec<String> = &mut ws;
    let wparam = LPARAM(wptr as isize);


    unsafe {
        EnumWindows(Some(enum_callback), wparam)?;
    }

    Ok(ws)
}


fn main() -> Result<()> {
    let ws = get_all_windows()?;

    for i in ws{
        println!("{i}");
    }

    Ok(())
}