mod app;
mod guest;
mod host_events;
mod tabbar;

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
use windows::core::Result as WinResult;
use winit::event_loop::EventLoop;
use winit::platform::windows::EventLoopBuilderExtWindows;

use crate::app::App;

fn main() -> WinResult<()> {
    guest::install_panic_restore_hook();

    let switch_requested = Arc::new(AtomicU32::new(0));
    let hook_switch_requested = Arc::clone(&switch_requested);

    let new_tab_requested = Arc::new(AtomicU32::new(0));
    let hook_new_tab_requested = Arc::clone(&new_tab_requested);

    let mut event_loop_builder = EventLoop::builder();
    event_loop_builder.with_msg_hook(move |message| {
        let message = unsafe { &*message.cast::<MSG>() };
        if message.message == WM_HOTKEY {
            if message.wParam.0 == app::SWITCH_HOTKEY_ID as usize {
                hook_switch_requested.fetch_add(1, Ordering::Release);
                return true;
            }
            if message.wParam.0 == app::NEW_TAB_HOTKEY_ID as usize {
                hook_new_tab_requested.fetch_add(1, Ordering::Release);
                return true;
            }
        }
        false
    });

    let event_loop = event_loop_builder.build().unwrap();
    let mut app = App::new(switch_requested, new_tab_requested);
    event_loop.run_app(&mut app).unwrap();

    Ok(())
}
