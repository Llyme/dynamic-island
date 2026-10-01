//! Work/activity-category detection -- port of `_check_work`/`categorize_work`
//! from `main.py`. Tracks which category (coding/writing/design/
//! communication/media/gaming/browsing/other/idle) currently has focus, and
//! confirms a real "session" (the one that gets a pill) only once a category
//! has held steady for a dwell period -- tab-switches and alt-tabs shouldn't
//! spawn a new session every couple of seconds. Ending an active session asks
//! for a longer grace period than starting one, same asymmetry as the Qt
//! version: a brief glance at Explorer/Slack shouldn't kill it.
//!
//! Every poll also feeds `activity` (per-day stats + recently-active
//! categories) for the hub's "Now" cards.

use crate::{exeinfo, IslandState};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

const POLL_MS: u64 = 2000;
const IDLE_THRESHOLD_S: f64 = 90.0;
const CONFIRM_S: f64 = 8.0;
const LEAVE_GRACE_S: f64 = 45.0;
const NON_SESSION: &[&str] = &["idle", "unknown", "other"];
// states that are not a focused-window category at all (nothing to record)
const NON_SESSION_STATES: &[&str] = &["idle", "unknown"];

const BROWSER_EXES: &[&str] = &["chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "arc"];

const APP_CATEGORY: &[(&str, &str)] = &[
    ("code", "coding"), ("code - insiders", "coding"), ("codium", "coding"),
    ("devenv", "coding"), ("pycharm64", "coding"), ("idea64", "coding"),
    ("clion64", "coding"), ("webstorm64", "coding"), ("rider64", "coding"),
    ("goland64", "coding"), ("phpstorm64", "coding"), ("sublime_text", "coding"),
    ("notepad++", "coding"), ("vim", "coding"), ("nvim", "coding"), ("cursor", "coding"),
    ("windowsterminal", "coding"), ("cmd", "coding"), ("powershell", "coding"),
    ("pwsh", "coding"), ("wt", "coding"), ("git-bash", "coding"), ("mintty", "coding"),
    ("androidstudio64", "coding"), ("postman", "coding"), ("dbeaver", "coding"),
    ("insomnia", "coding"), ("docker desktop", "coding"), ("gitkraken", "coding"),
    ("winword", "writing"), ("notion", "writing"), ("obsidian", "writing"),
    ("typora", "writing"), ("onenote", "writing"),
    ("figma", "design"), ("figmaagent", "design"), ("photoshop", "design"),
    ("illustrator", "design"), ("blender", "design"), ("inkscape", "design"),
    ("slack", "communication"), ("discord", "communication"), ("teams", "communication"),
    ("outlook", "communication"), ("thunderbird", "communication"), ("zoom", "communication"),
    ("spotify", "media"), ("vlc", "media"),
    ("steam", "gaming"), ("epicgameslauncher", "gaming"), ("battle.net", "gaming"),
];

const TITLE_KEYWORDS: &[(&str, &[&str])] = &[
    ("coding", &[
        "github.com", "gitlab.com", "stackoverflow.com", "localhost",
        "developer.mozilla", "docs.python.org", "leetcode", "bitbucket.org",
        "visual studio code", "readthedocs",
    ]),
    ("writing", &["docs.google.com", "notion.so", "overleaf.com"]),
    ("communication", &[
        "mail.google.com", "outlook.office.com", "discord.com",
        "web.whatsapp.com", "slack.com", "web.telegram.org",
    ]),
    ("media", &["youtube.com", "netflix.com", "twitch.tv", "reddit.com", "spotify.com"]),
];

fn categorize(exe_path: Option<&str>, title: &str) -> &'static str {
    let Some(path) = exe_path else { return "other" };
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase();
    let base = file.strip_suffix(".exe").unwrap_or(&file);

    if BROWSER_EXES.contains(&base) {
        let title_l = title.to_lowercase();
        for (category, keywords) in TITLE_KEYWORDS {
            if keywords.iter().any(|k| title_l.contains(k)) {
                return category;
            }
        }
        return "browsing";
    }

    APP_CATEGORY
        .iter()
        .find(|(exe, _)| *exe == base)
        .map(|(_, cat)| *cat)
        .unwrap_or("other")
}

pub(crate) fn label_for(category: &str) -> &'static str {
    match category {
        "coding" => "Coding",
        "writing" => "Writing",
        "design" => "Designing",
        "communication" => "Messaging",
        "media" => "Watching/Listening",
        "gaming" => "Gaming",
        "browsing" => "Browsing",
        "other" => "On the Computer",
        _ => "Away",
    }
}

fn idle_seconds() -> Option<f64> {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        return None;
    }
    let tick = unsafe { GetTickCount() };
    Some((tick.wrapping_sub(lii.dwTime) as f64 / 1000.0).max(0.0))
}

fn exe_path_for_pid(pid: u32) -> Option<String> {
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

/// (pid, exe_path, window_title) for whatever currently has focus.
fn foreground_window_info() -> Option<(u32, Option<String>, String)> {
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    let title = if len > 0 {
        let mut buf = vec![0u16; len as usize + 1];
        let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
        String::from_utf16_lossy(&buf[..written.max(0) as usize])
    } else {
        String::new()
    };
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    Some((pid, exe_path_for_pid(pid), title))
}

fn exe_display_name(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let stem = file.strip_suffix(".exe").or(file.strip_suffix(".EXE")).unwrap_or(file);
    stem.to_string()
}

#[derive(Serialize, Clone, Default)]
pub struct WorkSnapshot {
    pub has_session: bool,
    pub category: String,
    pub label: String,
    pub app_name: String,
    pub started_at_secs: f64, // seconds ago the session started, for the elapsed readout
}

pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || {
        let my_pid = unsafe { GetCurrentProcessId() };
        let mut candidate = String::from("idle");
        let mut candidate_since = Instant::now();
        let mut candidate_app_name = String::new();
        let mut confirmed: Option<String> = None;
        let mut confirmed_app_name = String::new();
        let mut confirmed_since = Instant::now();
        let mut last_tick = Instant::now();
        let mut last_flush = Instant::now();

        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));

            if !state.work_detection_enabled.load(Ordering::Relaxed) {
                if confirmed.is_some() {
                    confirmed = None;
                    candidate = "idle".to_string();
                    state.has_work.store(false, Ordering::Relaxed);
                    state.activity.set_session(None);
                    let _ = app.emit("work-tick", WorkSnapshot::default());
                }
                continue;
            }

            // every visible window keeps its category's card alive, focused or not
            // (a game/editor/browser on another monitor, or while the island has
            // focus). elapsed 0: stats and the session pill still follow focus.
            let mut seen: Vec<&'static str> = Vec::new();
            for win in crate::scan::visible_windows().iter().filter(|w| !w.minimized) {
                let cat = categorize(Some(&win.exe), &win.title);
                if cat == "other" || seen.contains(&cat) {
                    continue;
                }
                seen.push(cat);
                let name = exeinfo::lookup(&win.exe).name.unwrap_or_else(|| exe_display_name(&win.exe));
                state.activity.observe(cat, 0.0, &win.title, &name, Some(&win.exe));
            }

            let idle_s = idle_seconds();
            let mut title = String::new();
            let mut exe_path: Option<String> = None;
            let (new_state, app_name): (String, String) = if idle_s.map(|s| s >= IDLE_THRESHOLD_S).unwrap_or(false) {
                ("idle".to_string(), String::new())
            } else {
                match foreground_window_info() {
                    None => ("unknown".to_string(), String::new()),
                    Some((pid, _exe, _title)) if pid == my_pid => continue, // don't let glancing at our own hub count
                    Some((_pid, path, t)) => {
                        let cat = categorize(path.as_deref(), &t);
                        let name = path
                            .as_deref()
                            .map(|p| exeinfo::lookup(p).name.unwrap_or_else(|| exe_display_name(p)))
                            .unwrap_or_default();
                        title = t;
                        exe_path = path;
                        (cat.to_string(), name)
                    }
                }
            };

            // stats accounting is continuous (every poll, not gated behind the
            // session-confirmation dwell) -- a time tracker shouldn't lose the
            // first CONFIRM_S seconds of every switch
            let elapsed = last_tick.elapsed().as_secs_f64().min(POLL_MS as f64 / 1000.0 * 3.0);
            last_tick = Instant::now();
            if !NON_SESSION_STATES.contains(&new_state.as_str()) {
                state.activity.observe(&new_state, elapsed, &title, &app_name, exe_path.as_deref());
            }
            if last_flush.elapsed() >= Duration::from_secs(30) {
                last_flush = Instant::now();
                state.activity.flush();
            }

            if new_state != candidate {
                candidate = new_state.clone();
                candidate_since = Instant::now();
                candidate_app_name = app_name.clone();
            }

            let currently_in_session = confirmed
                .as_deref()
                .map(|c| !NON_SESSION.contains(&c))
                .unwrap_or(false);
            let required_dwell = if currently_in_session && NON_SESSION.contains(&new_state.as_str()) {
                LEAVE_GRACE_S
            } else {
                CONFIRM_S
            };

            let holding_long_enough = candidate_since.elapsed().as_secs_f64() >= required_dwell;
            if holding_long_enough && confirmed.as_deref() != Some(new_state.as_str()) {
                confirmed = Some(new_state.clone());
                confirmed_app_name = candidate_app_name.clone();
                confirmed_since = Instant::now();
            }

            let category = confirmed.clone().unwrap_or_else(|| "idle".to_string());
            let has_session = !NON_SESSION.contains(&category.as_str());
            state.has_work.store(has_session, Ordering::Relaxed);
            state
                .activity
                .set_session(has_session.then(|| (category.clone(), confirmed_since)));

            let _ = app.emit(
                "work-tick",
                WorkSnapshot {
                    has_session,
                    label: label_for(&category).to_string(),
                    category,
                    app_name: confirmed_app_name.clone(),
                    started_at_secs: confirmed_since.elapsed().as_secs_f64(),
                },
            );
        }
    });
}
