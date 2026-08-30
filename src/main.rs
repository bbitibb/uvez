#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod guest;
mod host_events;
mod icon;
mod logging;
mod tabbar;

use std::sync::{
    Arc,
    atomic::{AtomicIsize, AtomicU32, Ordering},
};

use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, MSG, WM_HOTKEY};
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

    let close_tab_requested = Arc::new(AtomicU32::new(0));
    let hook_close_tab_requested = Arc::clone(&close_tab_requested);

    let attach_requested = Arc::new(AtomicU32::new(0));
    let hook_attach_requested = Arc::clone(&attach_requested);
    let attach_target = Arc::new(AtomicIsize::new(0));
    let hook_attach_target = Arc::clone(&attach_target);

    let detach_requested = Arc::new(AtomicU32::new(0));
    let hook_detach_requested = Arc::clone(&detach_requested);

    let group_requested = Arc::new(AtomicU32::new(0));
    let hook_group_requested = Arc::clone(&group_requested);

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
            if message.wParam.0 == app::CLOSE_TAB_HOTKEY_ID as usize {
                hook_close_tab_requested.fetch_add(1, Ordering::Release);
                return true;
            }
            if message.wParam.0 == app::ATTACH_HOTKEY_ID as usize {
                let foreground = unsafe { GetForegroundWindow() };
                hook_attach_target.store(foreground.0 as isize, Ordering::Release);
                hook_attach_requested.fetch_add(1, Ordering::Release);
                return true;
            }
            if message.wParam.0 == app::DETACH_HOTKEY_ID as usize {
                hook_detach_requested.fetch_add(1, Ordering::Release);
                return true;
            }
            if message.wParam.0 == app::GROUP_HOTKEY_ID as usize {
                hook_group_requested.fetch_add(1, Ordering::Release);
                return true;
            }
        }
        false
    });

    let event_loop = event_loop_builder.build().unwrap();
    let mut app = App::new(
        switch_requested,
        new_tab_requested,
        close_tab_requested,
        attach_requested,
        attach_target,
        detach_requested,
        group_requested,
    );
    event_loop.run_app(&mut app).unwrap();

    if app.take_fatal_error().is_some() {
        std::process::exit(1);
    }

    Ok(())
}
