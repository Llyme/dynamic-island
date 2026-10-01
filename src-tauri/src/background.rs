//! Custom island backgrounds: the user picks an image or video for the
//! collapsed island and (optionally) a different one for the expanded hub.
//! The chosen file is copied into `%APPDATA%\NADI\backgrounds\` so it
//! keeps working if the original moves, and the webview loads it through
//! Tauri's asset protocol (which streams video with range requests).

use crate::settings::{self, Settings};
use crate::IslandState;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};

/// true while the native file dialog is open: it steals focus from the island,
/// which must not read as "clicked outside" and collapse the hub
pub static PICKING: AtomicBool = AtomicBool::new(false);

const MAX_BYTES: u64 = 1 << 30; // 1 GiB
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "mp4", "webm", "mov", "m4v"];

fn backgrounds_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("NADI").join("backgrounds"))
}

fn pick_file(owner: isize) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let filter: Vec<u16> =
        "Images and videos\0*.png;*.jpg;*.jpeg;*.gif;*.webp;*.bmp;*.avif;*.mp4;*.webm;*.mov;*.m4v\0All files\0*.*\0\0"
            .encode_utf16()
            .collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: HWND(owner as *mut _),
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY,
        ..Default::default()
    };
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return None; // cancelled
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

fn field_mut<'a>(s: &'a mut Settings, kind: &str) -> Result<&'a mut String, String> {
    match kind {
        "compact" => Ok(&mut s.bg_compact),
        "hub" => Ok(&mut s.bg_hub),
        other => Err(format!("unknown background kind: {other}")),
    }
}

/// only ever delete files that live in our own backgrounds folder
fn remove_owned(path: &str) {
    if path.is_empty() {
        return;
    }
    let (Some(dir), p) = (backgrounds_dir(), Path::new(path)) else { return };
    if p.starts_with(&dir) {
        let _ = std::fs::remove_file(p);
    }
}

#[tauri::command]
pub async fn pick_background(window: WebviewWindow, kind: String) -> Result<Option<Settings>, String> {
    field_mut(&mut Settings::default(), &kind)?; // validate kind up front
    // owner = the island itself, so the (topmost) island doesn't cover the dialog
    let owner = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    PICKING.store(true, Ordering::Relaxed);
    let picked = tauri::async_runtime::spawn_blocking(move || pick_file(owner)).await;
    PICKING.store(false, Ordering::Relaxed);
    let Some(src) = picked.map_err(|e| e.to_string())? else { return Ok(None) };

    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return Err("Unsupported file type -- use png, jpg, gif, webp, mp4 or webm".into());
    }
    let len = std::fs::metadata(&src).map_err(|e| e.to_string())?.len();
    if len > MAX_BYTES {
        return Err("File is larger than 1 GB".into());
    }
    let dir = backgrounds_dir().ok_or("no APPDATA")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem: String = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("background")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == ' ' { c } else { '_' })
        .take(40)
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // unique name per pick: a replaced video is never held open/cached under the old name
    let dest = dir.join(format!("{kind}-{stamp}-{stem}.{ext}"));
    std::fs::copy(&src, &dest).map_err(|e| e.to_string())?;

    let state = window.state::<Arc<IslandState>>();
    let mut guard = state.settings.lock().unwrap();
    let field = field_mut(&mut guard, &kind)?;
    let old = std::mem::replace(field, dest.to_string_lossy().into_owned());
    remove_owned(&old);
    settings::save_to_disk(&guard);
    Ok(Some(guard.clone()))
}

#[tauri::command]
pub fn clear_background(window: WebviewWindow, kind: String) -> Result<Settings, String> {
    let state = window.state::<Arc<IslandState>>();
    let mut guard = state.settings.lock().unwrap();
    let field = field_mut(&mut guard, &kind)?;
    let old = std::mem::take(field);
    remove_owned(&old);
    settings::save_to_disk(&guard);
    Ok(guard.clone())
}
