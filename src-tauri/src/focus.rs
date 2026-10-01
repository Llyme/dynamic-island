//! "Go to the source": clicking a hub card brings the app behind it to the
//! front (the player for a media card, the game, the editor/browser for a
//! work card, Discord/Viber for a notification).

use crate::game::exe_path_for_pid;
use std::collections::HashMap;
use windows::core::w;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, GWL_EXSTYLE,
    GW_OWNER, SW_RESTORE, SW_SHOWNORMAL, WS_EX_TOOLWINDOW,
};

struct Cand {
    hwnd: HWND,
    title: String,
    /// lower-cased full exe path
    exe: String,
}

impl Cand {
    fn stem(&self) -> &str {
        let file = self.exe.rsplit(['\\', '/']).next().unwrap_or(&self.exe);
        file.strip_suffix(".exe").unwrap_or(file)
    }
}

/// Top-level, visible, titled, un-owned windows in z-order (most recently
/// used first), minus the island's own.
fn candidates() -> Vec<Cand> {
    struct Ctx {
        out: Vec<Cand>,
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
            .or_insert_with(|| exe_path_for_pid(pid).unwrap_or_default().to_lowercase())
            .clone();
        if exe.is_empty() {
            return BOOL(1);
        }
        ctx.out.push(Cand { hwnd: h, title: String::from_utf16_lossy(&buf[..n.min(256)]), exe });
        BOOL(1)
    }
    let mut ctx = Ctx { out: Vec::new(), exes: HashMap::new(), me: std::process::id() };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out
}

/// SetForegroundWindow is refused for background processes unless the input
/// queues are attached to the current foreground thread first.
fn raise(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let fg = GetForegroundWindow();
        let fg_tid = if fg.is_invalid() { 0 } else { GetWindowThreadProcessId(fg, None) };
        let me = GetCurrentThreadId();
        let attached = fg_tid != 0 && fg_tid != me && AttachThreadInput(me, fg_tid, true).as_bool();
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        if attached {
            let _ = AttachThreadInput(me, fg_tid, false);
        }
    }
}

/// what an SMTC source id ("Spotify.exe", "Chrome", "MSEdge",
/// "Microsoft.ZuneMusic_8wekyb3d8bbwe!App") is likely to be called as an exe
fn source_token(source: &str) -> String {
    let s = source.to_lowercase();
    let s = s.split('!').next().unwrap_or(&s);
    let s = s.split('_').next().unwrap_or(s);
    let s = s.rsplit('.').find(|p| *p != "exe").unwrap_or(s);
    s.to_string()
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Focus the window that best matches: an exe path (games, work apps), a
/// window-title hint (a project name, or a media title -- browsers show the
/// tab title), and/or an SMTC source app id. Returns whether one was found.
#[tauri::command]
pub fn focus_source(exe_path: Option<String>, title_hint: Option<String>, source_id: Option<String>) -> bool {
    let cands = candidates();
    let hint = title_hint.as_deref().map(str::trim).filter(|h| h.chars().count() >= 3);
    let by_hint = |c: &&Cand| hint.map_or(false, |h| contains_ci(&c.title, h));

    let pick: Option<&Cand> = if let Some(exe) = exe_path.as_deref().filter(|e| !e.is_empty()) {
        let exe = exe.to_lowercase();
        let all: Vec<&Cand> = cands.iter().filter(|c| c.exe == exe).collect();
        all.iter().copied().find(by_hint).or_else(|| all.first().copied())
    } else {
        let token = source_id.as_deref().map(source_token).filter(|t| t.len() >= 3);
        let by_source = |c: &&Cand| {
            token.as_deref().map_or(false, |t| {
                let stem = c.stem();
                stem.len() >= 3 && (stem.contains(t) || t.contains(stem))
            })
        };
        // best: the source app's window whose title also carries the hint
        cands
            .iter()
            .find(|c| by_source(c) && by_hint(c))
            .or_else(|| cands.iter().find(by_hint))
            .or_else(|| cands.iter().find(by_source))
    };
    match pick {
        Some(c) => {
            raise(c.hwnd);
            true
        }
        None => false,
    }
}

/// Focus an app's window by exe file name, or -- when it only lives in the
/// tray -- open it through its URL scheme.
pub fn focus_or_open(exe_name: &str, url: windows::core::PCWSTR) {
    let found = candidates().iter().find(|c| c.exe.ends_with(exe_name)).map(|c| c.hwnd);
    match found {
        Some(h) => raise(h),
        None => unsafe {
            ShellExecuteW(None, w!("open"), url, None, None, SW_SHOWNORMAL);
        },
    }
}
