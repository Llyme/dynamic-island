//! "What are you browsing": for the hub's browsing card. The window title only
//! names the page, so the rest comes from the browser's own history database
//! (Chromium family: Chrome, Edge, Brave, Vivaldi, Opera). The live file is
//! locked by the browser, so a copy is read -- only while the hub is open and a
//! browser is in use, and only when the file changed since the last copy.

use crate::IslandState;
use crate::pagetext::{self, PagePreview};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const REFRESH_S: u64 = 12;
/// gap between the Windows FILETIME-style epoch Chromium uses (1601) and Unix
const CHROME_EPOCH_OFFSET_US: i64 = 11_644_473_600_000_000;

#[derive(Serialize, Clone, Default)]
pub struct BrowseInfo {
    /// site of the page in front of you, when it could be matched in the history
    pub domain: Option<String>,
    /// what the page says, read from the window
    pub preview: Option<PagePreview>,
}

/// "Rust docs - Google Chrome - Michael" -> "Rust docs"
pub fn clean_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut cut = title.len();
    for marker in [
        " - google chrome",
        " - microsoft edge",
        " - microsoft\u{200b} edge",
        " - brave",
        " - vivaldi",
        " - opera",
        " \u{2014} mozilla firefox",
        " - mozilla firefox",
    ] {
        if let Some(i) = lower.find(marker) {
            cut = cut.min(i);
        }
    }
    let mut t = title[..cut].trim().to_string();
    // Edge: "Page and 3 more pages - Profile" (the tab count and profile trail the title)
    if let Some(i) = t.rfind(" and ") {
        let mut words = t[i + 5..].split_whitespace();
        let counted = words.next().map_or(false, |n| n.chars().all(|c| c.is_ascii_digit()));
        if counted && words.next() == Some("more") {
            t.truncate(i);
        }
    }
    t
}

fn domain_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?.split(':').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Newest `History` file among a Chromium browser's profiles.
pub(crate) fn history_path(exe: &str) -> Option<PathBuf> {
    let file = exe.rsplit(['\\', '/']).next()?.to_lowercase();
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
    let data_dir = match file.strip_suffix(".exe").unwrap_or(&file) {
        "chrome" => local.join("Google/Chrome/User Data"),
        "msedge" => local.join("Microsoft/Edge/User Data"),
        "brave" => local.join("BraveSoftware/Brave-Browser/User Data"),
        "vivaldi" => local.join("Vivaldi/User Data"),
        "opera" => roaming?.join("Opera Software/Opera Stable"),
        _ => return None,
    };
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut consider = |p: PathBuf| {
        if let Ok(t) = std::fs::metadata(&p).and_then(|m| m.modified()) {
            if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
                best = Some((t, p));
            }
        }
    };
    consider(data_dir.join("History")); // Opera keeps it at the top
    if let Ok(rd) = std::fs::read_dir(&data_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "Default" || name.starts_with("Profile ") {
                consider(e.path().join("History"));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn midnight_chrome_us() -> i64 {
    use chrono::{Local, TimeZone};
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    let unix = Local.from_local_datetime(&midnight).earliest().map_or(0, |t| t.timestamp());
    unix * 1_000_000 + CHROME_EPOCH_OFFSET_US
}

/// Copy the locked database (and its WAL, if any) so it can be opened.
fn snapshot(src: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("nadi-history");
    std::fs::create_dir_all(&dir).ok()?;
    let dst = dir.join("History");
    std::fs::copy(src, &dst).ok()?;
    let wal = src.with_file_name("History-wal");
    let dst_wal = dir.join("History-wal");
    if wal.exists() {
        let _ = std::fs::copy(&wal, &dst_wal);
    } else {
        let _ = std::fs::remove_file(&dst_wal);
    }
    Some(dst)
}

/// The site of the newest visit whose title matches the page in front of you.
fn query(db: &Path, page_title: &str) -> Option<String> {
    if page_title.is_empty() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT u.url FROM visits v JOIN urls u ON u.id = v.url \
             WHERE v.visit_time >= ?1 AND u.title = ?2 AND (v.transition & 255) NOT IN (3, 4) \
             ORDER BY v.visit_time DESC LIMIT 1",
        )
        .ok()?;
    let url: String = stmt.query_row(rusqlite::params![midnight_chrome_us(), page_title], |r| r.get(0)).ok()?;
    domain_of(&url)
}

/// Card button -> click the page's button of that name (see `pagetext::click_button`).
#[tauri::command]
pub async fn click_page_button(window: tauri::WebviewWindow, label: String, x: f32, y: f32) -> bool {
    use tauri::Manager;
    let state = window.state::<Arc<IslandState>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some((exe, title)) = state.activity.active_browser() else { return false };
        let target = crate::scan::visible_windows().into_iter().find(|w| {
            !w.minimized && w.exe.eq_ignore_ascii_case(&exe) && (title.is_empty() || w.title.contains(&title))
        });
        target.map_or(false, |w| pagetext::click_button(w.hwnd, &label, x, y))
    })
    .await
    .unwrap_or(false)
}

pub fn spawn(state: Arc<IslandState>) {
    std::thread::spawn(move || {
        // background work: never compete with the UI for CPU
        unsafe {
            let _ = windows::Win32::System::Threading::SetThreadPriority(
                windows::Win32::System::Threading::GetCurrentThread(),
                windows::Win32::System::Threading::THREAD_PRIORITY_BELOW_NORMAL,
            );
        }
        let mut last_key = String::new();
        let mut last_at = std::time::Instant::now() - Duration::from_secs(REFRESH_S);
        let mut last_mtime: Option<SystemTime> = None;
        let mut db: Option<PathBuf> = None;
        let mut last_fp: Option<u64> = None;
        let mut last_preview: Option<PagePreview> = None;
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            if !state.hub_open.load(Ordering::Relaxed) {
                continue;
            }
            let Some((exe, title)) = state.activity.active_browser() else {
                *state.activity.browsing.lock().unwrap() = None;
                continue;
            };
            let key = format!("{exe}|{title}");
            let page_changed = key != last_key;
            if !page_changed && last_at.elapsed() < Duration::from_secs(REFRESH_S) {
                continue;
            }
            last_key = key;
            last_at = std::time::Instant::now();
            if page_changed {
                last_fp = None;
                last_preview = None;
            }

            // the site, from the browser history (Chromium family only)
            let mut domain = None;
            if let Some(src) = history_path(&exe) {
                let mtime = std::fs::metadata(&src).and_then(|m| m.modified()).ok();
                // the copy is the expensive part: redo it only when the file changed
                if mtime != last_mtime || db.is_none() {
                    db = snapshot(&src);
                    last_mtime = mtime;
                }
                domain = db.as_deref().and_then(|d| query(d, &title));
            }

            // what the page says, read from the window
            if state.settings.lock().unwrap().page_preview {
                let target = crate::scan::visible_windows().into_iter().find(|w| {
                    !w.minimized && w.exe.eq_ignore_ascii_case(&exe) && (title.is_empty() || w.title.contains(&title))
                });
                if let Some(w) = target {
                    if let Some((preview, fp)) = pagetext::read_window(w.hwnd, &title, last_fp) {
                        last_fp = Some(fp);
                        // unchanged screen: keep the last reading
                        if preview.is_some() || page_changed {
                            last_preview = preview;
                        }
                    }
                }
            }
            *state.activity.browsing.lock().unwrap() = Some(BrowseInfo { domain, preview: last_preview.clone() });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_the_browser_suffix() {
        assert_eq!(clean_title("Rust docs - Google Chrome"), "Rust docs");
        assert_eq!(clean_title("Inbox and 3 more pages - Personal - Microsoft\u{200b} Edge"), "Inbox");
        assert_eq!(clean_title("Plain"), "Plain");
    }

    #[test]
    fn domains_are_bare_hosts() {
        assert_eq!(domain_of("https://www.Example.com:8080/a?b#c").as_deref(), Some("example.com"));
        assert_eq!(domain_of("chrome://settings"), None);
    }
}
