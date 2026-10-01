//! Settings persistence + Windows autostart -- port of `load_settings`/
//! `save_settings`/`set_autostart` from `main.py` and `platform_backend/windows.py`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};

use crate::IslandState;

const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_NAME: &str = "NADI";

// the app used to be called "Dynamic Island": its data folder and autostart entry are carried over
const DATA_DIR: &str = "NADI";
const OLD_DATA_DIR: &str = "DynamicIsland";
const OLD_AUTOSTART_NAME: &str = "DynamicIsland";

// struct-level default: an older settings.json missing newer fields still loads
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub game_detection: bool,
    pub work_detection: bool,
    /// list what the browsers, Steam and qBittorrent are downloading right now
    pub download_detection: bool,
    /// list the Claude Code sessions running on this machine as cards
    pub llm_detection: bool,
    /// the island drops down briefly when a Claude Code session needs you or finishes
    pub llm_brief: bool,
    pub start_with_windows: bool,
    pub idle_hide_delay_s: u64,
    pub peek_duration_s: u64,
    /// how long the cursor must rest at the top edge before the island appears (ms, 0 = at once)
    pub edge_dwell_ms: u64,
    /// while pinned and left alone (for the hide delay) the island shrinks to this share of its size, 100 = never
    pub pin_shrink: u64,
    /// width in logical px of the standard collapsed island (the media and work pills, the banner, the session
    /// pill); the idle and game pills keep their proportions. (Older files hold a percentage: see `compact_px`.)
    pub compact_width: u64,
    /// width of the expanded island (the hub), in logical pixels
    pub hub_width: u64,
    /// how far below the top edge of the screen the island sits, in logical pixels
    pub top_margin: u64,
    /// reveal under the cursor (vs. screen center) and glide along with it at the top edge
    pub show_at_cursor: bool,
    pub cursor_follow: bool,
    /// eyes react to system audio (WASAPI loopback, analyzed locally, never stored)
    pub react_to_audio: bool,
    /// draw the eyes (the audio light shows either way)
    pub show_eyes: bool,
    /// theme (accent) colour as #rrggbb
    pub accent_color: String,
    /// the island says what time it is now and then (a session pill)
    pub time_announce: bool,
    /// which step of `clock::INTERVALS`
    pub time_interval: u64,
    pub time_24h: bool,
    /// 0..100 -- how strongly (and how far) the sound's light bleeds outside the island, 0 = off
    pub audio_bleed: u64,
    pub calendar_ics_url: String,
    pub calendar_reminder_lead_min: u64,
    pub calendar_poll_min: u64,
    /// custom backgrounds (files copied into %APPDATA%\NADI\backgrounds): the
    /// collapsed island, and optionally a different one for the expanded hub
    pub bg_compact: String,
    pub bg_hub: String,
    /// 0..90 -- how much black is laid over the background so text stays readable
    pub bg_dim: u64,
    /// 0..100 -- how strongly (and how far) the glow around the brief-show views shines, 0 = off
    pub glow_intensity: u64,
    /// browsing card: fetch the page in front of you (public https pages only) to show its gist
    pub page_preview: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            game_detection: true,
            work_detection: true,
            download_detection: true,
            llm_detection: true,
            llm_brief: true,
            start_with_windows: false,
            idle_hide_delay_s: 1,
            peek_duration_s: 3,
            edge_dwell_ms: 300,
            pin_shrink: 75,
            compact_width: 260,
            hub_width: 420,
            top_margin: 4,
            show_at_cursor: true,
            cursor_follow: true,
            react_to_audio: true,
            show_eyes: true,
            accent_color: "#5ac88c".into(),
            time_announce: false,
            time_interval: 4,
            time_24h: false,
            audio_bleed: 60,
            calendar_ics_url: String::new(),
            calendar_reminder_lead_min: 15,
            calendar_poll_min: 10,
            bg_compact: String::new(),
            bg_hub: String::new(),
            bg_dim: 50,
            glow_intensity: 70,
            page_preview: true,
        }
    }
}

/// %APPDATA%\NADI: settings, work stats and custom backgrounds
pub(crate) fn data_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join(DATA_DIR))
}

fn settings_path() -> Option<PathBuf> {
    Some(data_dir()?.join("settings.json"))
}

/// Carry the old "Dynamic Island" install over: its data folder (custom backgrounds are stored
/// by absolute path, so those are rewritten too) and its "start with Windows" entry.
fn migrate_old_install() {
    use winreg::enums::*;
    use winreg::RegKey;

    if let Some(appdata) = std::env::var_os("APPDATA") {
        let (old, new) = (PathBuf::from(&appdata).join(OLD_DATA_DIR), PathBuf::from(&appdata).join(DATA_DIR));
        if old.exists() && !new.exists() && std::fs::rename(&old, &new).is_ok() {
            let file = new.join("settings.json");
            if let Ok(text) = std::fs::read_to_string(&file) {
                // (inside JSON the backslashes of a path are doubled)
                let fixed = text.replace("\\\\DynamicIsland\\\\", "\\\\NADI\\\\");
                if fixed != text {
                    let _ = std::fs::write(&file, fixed);
                }
            }
        }
    }

    // a dev build (cargo tauri dev) must not claim the entry: it would point at target\debug
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.to_string_lossy().contains("\\target\\") {
        return;
    }
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(AUTOSTART_KEY, KEY_SET_VALUE | KEY_QUERY_VALUE) else {
        return;
    };
    if key.get_value::<String, _>(OLD_AUTOSTART_NAME).is_ok() {
        let _ = key.delete_value(OLD_AUTOSTART_NAME);
        let _ = key.set_value(AUTOSTART_NAME, &format!("\"{}\"", exe.display()));
    }
}

/// The collapsed island's width used to be a percentage (60..160) of 260 px; it is pixels now.
/// Anything up to 160 is read as the old percentage.
pub fn compact_px(v: u64) -> u64 {
    (if v <= 160 { v * 260 / 100 } else { v }).clamp(180, 560)
}

pub fn load() -> Settings {
    migrate_old_install();
    let mut s: Settings = (|| {
        let path = settings_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    })()
    .unwrap_or_default();
    s.compact_width = compact_px(s.compact_width);
    s
}

pub(crate) fn save_to_disk(settings: &Settings) {
    let Some(path) = settings_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn set_autostart(enabled: bool) {
    use winreg::enums::*;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(AUTOSTART_KEY, KEY_SET_VALUE) else {
        return;
    };
    if enabled {
        if let Ok(exe) = std::env::current_exe() {
            let target = format!("\"{}\"", exe.display());
            let _ = key.set_value(AUTOSTART_NAME, &target);
        }
    } else {
        let _ = key.delete_value(AUTOSTART_NAME);
    }
}

#[tauri::command]
pub fn get_settings(window: WebviewWindow) -> Settings {
    let state = window.state::<Arc<IslandState>>();
    let settings = state.settings.lock().unwrap().clone();
    settings
}

#[tauri::command]
pub fn save_settings(window: WebviewWindow, settings: Settings) {
    let state = window.state::<Arc<IslandState>>();
    let (autostart_changed, calendar_changed) = {
        let current = state.settings.lock().unwrap();
        (
            current.start_with_windows != settings.start_with_windows,
            current.calendar_ics_url != settings.calendar_ics_url,
        )
    };
    if calendar_changed {
        state.calendar.refetch.store(true, Ordering::Relaxed);
    }
    if autostart_changed {
        set_autostart(settings.start_with_windows);
    }

    state
        .game_detection_enabled
        .store(settings.game_detection, Ordering::Relaxed);
    state
        .work_detection_enabled
        .store(settings.work_detection, Ordering::Relaxed);
    state
        .idle_hide_ms
        .store(settings.idle_hide_delay_s.max(1) * 1000, Ordering::Relaxed);

    state.edge_dwell_ms.store(settings.edge_dwell_ms.min(2000), Ordering::Relaxed);
    state.pin_shrink.store(settings.pin_shrink.clamp(30, 100), Ordering::Relaxed);
    state.compact_width.store(compact_px(settings.compact_width), Ordering::Relaxed);
    state.hub_width.store(settings.hub_width.clamp(340, 640), Ordering::Relaxed);
    state.top_margin.store(settings.top_margin.clamp(0, 80), Ordering::Relaxed);
    state.show_at_cursor.store(settings.show_at_cursor, Ordering::Relaxed);
    state.cursor_follow.store(settings.cursor_follow, Ordering::Relaxed);
    state.audio_enabled.store(settings.react_to_audio, Ordering::Relaxed);
    state
        .peek_ms
        .store(settings.peek_duration_s.max(1) * 1000, Ordering::Relaxed);

    save_to_disk(&settings);
    *state.settings.lock().unwrap() = settings;
}
