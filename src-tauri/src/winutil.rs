//! Thin wrappers around the handful of raw Win32 calls the island needs:
//! global cursor position (works even when the window doesn't have focus,
//! which Tauri/webview mouse events don't give you) and the geometry of
//! whichever monitor the cursor is currently on.

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

/// Ask Windows for 1 ms timer resolution. Without it a 16 ms sleep really lasts
/// 16 or 31 ms by turns, which makes every animation driven by the poll loop stutter.
pub fn fine_timer() {
    unsafe {
        timeBeginPeriod(1);
    }
}

/// Let mouse clicks fall through the whole window (or not). Tauri's own
/// `set_ignore_cursor_events` rebuilds the window's frame every time it is called
/// (SWP_FRAMECHANGED + ShowWindow), which shows as a twitch; this only flips the style
/// bits, and only when they are not already as wanted.
pub fn click_through(hwnd: isize, on: bool) {
    unsafe {
        let h = HWND(hwnd as *mut _);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let mut want = ex | WS_EX_LAYERED.0 as isize;
        if on {
            want |= WS_EX_TRANSPARENT.0 as isize;
        } else {
            want &= !(WS_EX_TRANSPARENT.0 as isize);
        }
        if want != ex {
            SetWindowLongPtrW(h, GWL_EXSTYLE, want);
        }
    }
}

/// Move and resize a window in one step. Doing the two separately lets a frame
/// show the new size at the old position, which reads as jitter during a resize.
pub fn place(hwnd: isize, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = SetWindowPos(
            HWND(hwnd as *mut _),
            None,
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// Put the window back on top of the topmost band, without moving, sizing or activating it.
/// `SetWindowPos` with `SWP_NOZORDER` (see `place`) never changes the stacking, so a window
/// that another topmost window (or the taskbar, a popup...) covered would stay covered.
pub fn raise(hwnd: isize) {
    unsafe {
        let _ = SetWindowPos(
            HWND(hwnd as *mut _),
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    #[allow(dead_code)]
    pub height: i32,
}

/// Whether the left mouse button is currently held, globally -- used to
/// detect a native `start_dragging()` move-loop ending, since that loop can
/// swallow the DOM `mouseup` the webview would otherwise get.
pub fn left_button_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

/// Global cursor position in physical screen pixels, regardless of which
/// window (if any) has focus.
pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// The work-area-free full geometry of the monitor under the given point
/// (physical pixels), falling back to the nearest monitor if the point
/// isn't over any (matches Qt's `QApplication.screenAt` + nearest fallback
/// behavior the Python version relied on).
pub fn monitor_geometry_at(x: i32, y: i32) -> Option<MonitorGeometry> {
    let pt = POINT { x, y };
    let hmon: HMONITOR = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if hmon.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info) };
    if ok.as_bool() {
        let RECT {
            left,
            top,
            right,
            bottom,
        } = info.rcMonitor;
        Some(MonitorGeometry {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    } else {
        None
    }
}
