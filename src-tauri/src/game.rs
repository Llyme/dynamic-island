//! Fullscreen-game detection -- the Rust port of `foreground_fullscreen_pid`
//! and friends from `platform_backend/windows.py`. A "game" candidate is a
//! foreground window that covers its whole monitor AND has no title bar
//! (`WS_CAPTION`): a maximized normal app (browser, editor) still carries
//! `WS_CAPTION` even filling the screen, while exclusive-fullscreen/
//! borderless games don't.

use crate::{exeinfo, IslandState};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, GWL_STYLE,
    WS_CAPTION,
};

const POLL_MS: u64 = 1500;

#[derive(Serialize, Clone, Default)]
pub struct GameSnapshot {
    pub has_game: bool,
    pub name: String,
    /// data: URL of the exe's icon
    pub icon: Option<String>,
    #[serde(skip)]
    pub exe_path: String,
    /// how many games are running in total (the pill shows "+N" past one)
    pub count: usize,
    /// process id of this game, to measure it (FPS, GPU, ...)
    pub pid: u32,
}

/// Covers its whole monitor and has no title bar: exclusive-fullscreen or
/// borderless-fullscreen, as games are (a maximized browser/IDE keeps its caption).
fn caption_less_fullscreen(hwnd: HWND) -> bool {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    let hmon: HMONITOR = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        return false;
    }
    let mon = mi.rcMonitor;
    let is_fullscreen = rect.left <= mon.left
        && rect.top <= mon.top
        && rect.right >= mon.right
        && rect.bottom >= mon.bottom;
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    is_fullscreen && style & WS_CAPTION.0 == 0
}

fn foreground_fullscreen_pid() -> Option<u32> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() || !caption_less_fullscreen(hwnd) {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

pub(crate) fn exe_path_for_pid(pid: u32) -> Option<String> {
    unsafe {
        let hproc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            hproc,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(hproc);
        if ok.is_err() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// Fallback when the exe has no ProductName/FileDescription: filename minus
/// extension, title-cased on separators.
fn display_name_from_path(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let stem = file.strip_suffix(".exe").or(file.strip_suffix(".EXE")).unwrap_or(file);
    let mut out = String::new();
    let mut cap_next = true;
    for ch in stem.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            cap_next = true;
        } else if cap_next {
            out.extend(ch.to_uppercase());
            cap_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

const IGNORE_EXE: &[&str] = &[
    "explorer.exe", "shellexperiencehost.exe", "searchhost.exe", "searchapp.exe", "startmenuexperiencehost.exe",
    "applicationframehost.exe", "textinputhost.exe", "lockapp.exe", "systemsettings.exe", "screenclippinghost.exe",
    "gamebar.exe", "widgets.exe",
];

/// Fullscreen video players, browsers (F11 / fullscreen video), presentations and
/// remote-desktop clients are caption-less fullscreen windows too, but they
/// are not games. Matched as substrings of the exe's file name.
const NOT_GAMES: &[&str] = &[
    "potplayer", "vlc", "mpv", "mpc-hc", "mpc-be", "wmplayer", "kmplayer", "gomplayer", "smplayer",
    "video.ui", "moviesandtv", "chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "zoom",
    "teams", "powerpnt", "acrord", "mstsc", "vmconnect", "obs64", "obs32",
];

fn is_not_game(exe_path: &str) -> bool {
    let file = exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path).to_lowercase();
    IGNORE_EXE.contains(&file.as_str()) || NOT_GAMES.iter().any(|n| file.contains(n))
}

fn snapshot_for(path: &str) -> GameSnapshot {
    let info = exeinfo::lookup(path);
    GameSnapshot {
        has_game: true,
        name: info.name.unwrap_or_else(|| display_name_from_path(path)),
        icon: info.icon,
        exe_path: path.to_string(),
        count: 1,
        pid: 0,
    }
}

/// Every fullscreen, caption-less window on any screen is "a game" -- focused
/// or not (a game on the other monitor while you use the island stays
/// detected). The focused one comes first, then front-to-back z-order.
fn detect_games() -> Vec<(u32, GameSnapshot)> {
    let mut out: Vec<(u32, GameSnapshot)> = Vec::new();
    if let Some(pid) = foreground_fullscreen_pid() {
        if let Some(path) = exe_path_for_pid(pid).filter(|p| !is_not_game(p)) {
            out.push((pid, snapshot_for(&path)));
        }
    }
    for w in crate::scan::visible_windows() {
        if w.minimized || out.iter().any(|(p, _)| *p == w.pid) || is_not_game(&w.exe) {
            continue;
        }
        if caption_less_fullscreen(w.hwnd) {
            out.push((w.pid, snapshot_for(&w.exe)));
        }
    }
    out
}

/// Whether a process is still running (STILL_ACTIVE == 259).
pub(crate) fn pid_alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok && code == 259
    }
}

struct Running {
    pid: u32,
    snap: GameSnapshot,
    since: Instant,
}

pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || {
        // Sessions outlive focus: alt-tabbing to a browser/Discord mid-game
        // must not end one. A game ends when its process exits (or detection
        // is switched off). Several games can run at once -- most recently
        // focused first; the pill shows the front one, the hub lists them all.
        let mut running: Vec<Running> = Vec::new();
        loop {
            if !state.game_detection_enabled.load(Ordering::Relaxed) {
                running.clear();
            } else {
                // back to front, so the frontmost game ends up first
                for (pid, snap) in detect_games().into_iter().rev() {
                    match running.iter().position(|r| r.pid == pid) {
                        Some(i) => {
                            let mut r = running.remove(i);
                            r.snap = snap;
                            running.insert(0, r);
                        }
                        None => running.insert(0, Running { pid, snap, since: Instant::now() }),
                    }
                }
                running.retain(|r| pid_alive(r.pid) && !is_not_game(&r.snap.exe_path));
            }

            let mut snap = running.first().map(|r| r.snap.clone()).unwrap_or_default();
            snap.count = running.len();
            snap.pid = running.first().map_or(0, |r| r.pid);
            state.has_game.store(snap.has_game, Ordering::Relaxed);
            state.activity.set_games(
                running
                    .iter()
                    .map(|r| (r.snap.name.clone(), r.snap.exe_path.clone(), r.since, r.pid))
                    .collect(),
            );
            let _ = app.emit("game-tick", snap);
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    });
}
