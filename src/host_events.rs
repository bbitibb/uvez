use std::ffi::c_void;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{DefSubclassProc, SUBCLASSPROC};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, HTBOTTOM, HTBOTTOMLEFT,
    HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST,
    HWND_TOPMOST, IsZoomed, NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYSIZEFRAME,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, WM_CANCELMODE, WM_ENTERSIZEMOVE,
    WM_EXITSIZEMOVE, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WS_EX_TOPMOST,
};

use crate::guest::get_window_process_id;

pub(crate) const HOST_SUBCLASS_ID: usize = 1;
pub(crate) const MAX_VISIBLE_GUESTS: usize = 4;

pub(crate) const HOST_SUBCLASS_PROC: SUBCLASSPROC = Some(host_subclass_proc);

#[derive(Default)]
pub(crate) struct GuestSlot {
    pub(crate) hwnd: AtomicIsize,
    pub(crate) pid: AtomicU32,
}

#[derive(Default)]
struct LiftSlot {
    hwnd: AtomicIsize,
    pid: AtomicU32,
    was_topmost: AtomicBool,
}

#[derive(Default)]
pub(crate) struct NativeHostEvents {
    pub(crate) visible: [GuestSlot; MAX_VISIBLE_GUESTS],
    lifted: [LiftSlot; MAX_VISIBLE_GUESTS],
    pub(crate) in_size_move: AtomicBool,
    pub(crate) size_move_finished: AtomicBool,
    pub(crate) subclass_ref_released: AtomicBool,
}

impl NativeHostEvents {
    pub(crate) fn set_visible(&self, guests: &[(isize, u32)]) {
        for (slot_index, slot) in self.visible.iter().enumerate() {
            match guests.get(slot_index) {
                Some(&(hwnd, pid)) => {
                    slot.hwnd.store(hwnd, Ordering::Release);
                    slot.pid.store(pid, Ordering::Release);
                }
                None => {
                    slot.hwnd.store(0, Ordering::Release);
                    slot.pid.store(0, Ordering::Release);
                }
            }
        }
    }

    pub(crate) fn clear_visible(&self) {
        self.set_visible(&[]);
    }

    pub(crate) fn has_lifted(&self) -> bool {
        self.lifted
            .iter()
            .any(|lift| lift.hwnd.load(Ordering::Acquire) != 0)
    }
}

fn hwnd_matches_pid(hwnd: HWND, expected_pid: u32) -> bool {
    expected_pid != 0 && get_window_process_id(hwnd) == expected_pid
}

fn frame_thickness(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96);
        let padded = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        let frame_x = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + padded;
        let frame_y = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + padded;
        (frame_x, frame_y)
    }
}

fn lift_visible_guests(events: &NativeHostEvents, host_hwnd: HWND) {
    if unsafe { GetForegroundWindow() } != host_hwnd {
        return;
    }

    if !release_lifted_guest(events) {
        return;
    }

    for slot in &events.visible {
        let hwnd_value = slot.hwnd.load(Ordering::Acquire);
        if hwnd_value == 0 {
            continue;
        }

        let hwnd = HWND(hwnd_value as *mut c_void);
        let pid = slot.pid.load(Ordering::Acquire);
        if !hwnd_matches_pid(hwnd, pid) {
            continue;
        }

        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 };
        let was_topmost = ex_style & WS_EX_TOPMOST.0 != 0;
        let lifted = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };

        if lifted.is_ok()
            && let Some(lift) = events
                .lifted
                .iter()
                .find(|lift| lift.hwnd.load(Ordering::Acquire) == 0)
        {
            lift.was_topmost.store(was_topmost, Ordering::Release);
            lift.pid.store(pid, Ordering::Release);
            lift.hwnd.store(hwnd_value, Ordering::Release);
        }
    }
}

pub(crate) fn release_lifted_guest(events: &NativeHostEvents) -> bool {
    let mut all_released = true;

    for lift in &events.lifted {
        let hwnd_value = lift.hwnd.load(Ordering::Acquire);
        if hwnd_value == 0 {
            continue;
        }

        let hwnd = HWND(hwnd_value as *mut c_void);
        let pid = lift.pid.load(Ordering::Acquire);
        if !hwnd_matches_pid(hwnd, pid) {
            lift.hwnd.store(0, Ordering::Release);
            continue;
        }

        let insert_after = if lift.was_topmost.load(Ordering::Acquire) {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        let released = unsafe {
            SetWindowPos(
                hwnd,
                Some(insert_after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };

        if released.is_ok() {
            lift.hwnd.store(0, Ordering::Release);
        } else {
            all_released = false;
        }
    }

    all_released
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
            WM_NCCALCSIZE if wparam.0 != 0 => {
                if (unsafe { IsZoomed(hwnd) }).as_bool() {
                    let (frame_x, frame_y) = frame_thickness(hwnd);
                    let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
                    let rect = &mut params.rgrc[0];
                    rect.left += frame_x;
                    rect.top += frame_y;
                    rect.right -= frame_x;
                    rect.bottom -= frame_y;
                }
                return LRESULT(0);
            }
            WM_NCHITTEST if !(unsafe { IsZoomed(hwnd) }).as_bool() => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let mut rect = RECT::default();
                let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
                let (frame_x, frame_y) = frame_thickness(hwnd);

                let left = x < rect.left + frame_x;
                let right = x >= rect.right - frame_x;
                let top = y < rect.top + frame_y;
                let bottom = y >= rect.bottom - frame_y;

                let hit = match (left, right, top, bottom) {
                    (true, _, true, _) => HTTOPLEFT,
                    (_, true, true, _) => HTTOPRIGHT,
                    (true, _, _, true) => HTBOTTOMLEFT,
                    (_, true, _, true) => HTBOTTOMRIGHT,
                    (true, _, _, _) => HTLEFT,
                    (_, true, _, _) => HTRIGHT,
                    (_, _, true, _) => HTTOP,
                    (_, _, _, true) => HTBOTTOM,
                    _ => HTCLIENT,
                };

                return LRESULT(hit as isize);
            }
            WM_ENTERSIZEMOVE => {
                events.size_move_finished.store(false, Ordering::Release);
                events.in_size_move.store(true, Ordering::Release);
                lift_visible_guests(events, hwnd);
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

pub(crate) unsafe fn release_subclass_reference(reference_data: usize) {
    let pointer = reference_data as *const NativeHostEvents;
    let events = unsafe { &*pointer };

    if !events.subclass_ref_released.swap(true, Ordering::AcqRel) {
        drop(unsafe { Arc::from_raw(pointer) });
    }
}
