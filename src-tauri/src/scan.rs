//! Every window the user could be looking at -- not just the focused one. Game,
//! work and browsing detection read this so they keep working while another
//! window (or the island itself) has focus.

use crate::game::exe_path_for_pid;
use std::collections::HashMap;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

pub struct Win {
    pub hwnd: HWND,
    pub pid: u32,
    pub exe: String,
    pub title: String,
    pub minimized: bool,
}

/// Top-level, visible, titled, un-owned, non-cloaked windows in z-order (front
/// first), minus the island's own. Cloaked windows (other virtual desktops,
/// suspended UWP frames) are on no screen, so they are skipped.
pub fn visible_windows() -> Vec<Win> {
    struct Ctx {
        out: Vec<Win>,
        exes: HashMap<u32, String>,
        me: u32,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if !IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).map_or(false, |o| !o.is_invalid()) {
            return BOOL(1);
        }
        if GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return BOOL(1);
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return BOOL(1);
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, &mut buf) as usize;
        if n == 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == 0 || pid == ctx.me {
            return BOOL(1);
        }
        let exe = ctx
            .exes
            .entry(pid)
            .or_insert_with(|| exe_path_for_pid(pid).unwrap_or_default())
            .clone();
        if exe.is_empty() {
            return BOOL(1);
        }
        ctx.out.push(Win {
            hwnd: h,
            pid,
            exe,
            title: String::from_utf16_lossy(&buf[..n.min(256)]),
            minimized: IsIconic(h).as_bool(),
        });
        BOOL(1)
    }
    let mut ctx = Ctx { out: Vec::new(), exes: HashMap::new(), me: std::process::id() };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out
}
