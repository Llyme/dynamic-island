//! "What are you working on": for the hub's coding card. Finds the folder of the
//! project open in the editor (from VS Code-family `storage.json`), then asks git
//! for the branch, the files with uncommitted changes and the last commit. The
//! refresh only runs while the hub is open, every few seconds, and spawns
//! `git` without a console window -- a few milliseconds of work.

use crate::IslandState;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

const REFRESH_S: u64 = 6;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize, Clone, Default)]
pub struct FileChange {
    /// file name only
    pub name: String,
    /// path relative to the repo root, for the tooltip
    pub path: String,
    pub added: u32,
    pub deleted: u32,
    /// 'M' modified, 'A' added/untracked, 'D' deleted
    pub status: char,
}

#[derive(Serialize, Clone, Default)]
pub struct ProjectInfo {
    pub name: String,
    pub branch: Option<String>,
    /// changed files, most recently touched first (max MAX_FILES)
    pub files: Vec<FileChange>,
    /// how many files are changed in total
    pub changed: usize,
    /// lines added/removed across all uncommitted changes
    pub added: u32,
    pub deleted: u32,
}

const MAX_FILES: usize = 5;
/// untracked files are counted line by line: cap how many / how big
const MAX_UNTRACKED: usize = 300;
const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;

/// file:///c%3A/Workspace/x -> C:\Workspace\x
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file:///")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let s = String::from_utf8(bytes).ok()?;
    Some(PathBuf::from(s.replace('/', "\\")))
}

/// Workspace folders VS Code-family editors know about, the one that was last
/// active first.
fn editor_folders() -> Vec<PathBuf> {
    let Some(appdata) = std::env::var_os("APPDATA") else { return Vec::new() };
    let appdata = PathBuf::from(appdata);
    let mut out: Vec<PathBuf> = Vec::new();
    for app in ["Code", "Code - Insiders", "Cursor", "VSCodium", "Windsurf"] {
        let p = appdata.join(app).join("User").join("globalStorage").join("storage.json");
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let ws = &v["windowsState"];
        let mut uris: Vec<String> = Vec::new();
        if let Some(f) = ws["lastActiveWindow"]["folder"].as_str() {
            uris.push(f.to_string());
        }
        if let Some(list) = ws["openedWindows"].as_array() {
            uris.extend(list.iter().filter_map(|w| w["folder"].as_str().map(str::to_string)));
        }
        if let Some(map) = v["profileAssociations"]["workspaces"].as_object() {
            uris.extend(map.keys().cloned());
        }
        out.extend(uris.iter().filter_map(|u| uri_to_path(u)));
    }
    out
}

fn find_project_dir(name: &str) -> Option<PathBuf> {
    editor_folders().into_iter().find(|p| {
        p.file_name().map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case(name)) && p.is_dir()
    })
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// "<added>	<deleted>	<path>" lines from `git diff --numstat` ("-" for binary)
fn parse_numstat(text: &str) -> Vec<(u32, u32, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let a = it.next()?;
            let d = it.next()?;
            let path = it.next()?.trim_matches('"').to_string();
            Some((a.parse().unwrap_or(0), d.parse().unwrap_or(0), path))
        })
        .collect()
}

fn count_lines(p: &Path) -> Option<u32> {
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_file() || meta.len() > MAX_UNTRACKED_BYTES {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    if bytes.contains(&0) {
        return None; // binary
    }
    let n = bytes.iter().filter(|&&b| b == b'\n').count() as u32;
    Some(if bytes.last().map_or(false, |&b| b != b'\n') { n + 1 } else { n })
}

fn gather(name: &str) -> ProjectInfo {
    let mut info = ProjectInfo { name: name.to_string(), ..Default::default() };
    let Some(dir) = find_project_dir(name) else { return info };
    info.branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| !b.is_empty());
    if info.branch.is_none() {
        return info; // not a git repo
    }

    // uncommitted changes to tracked files (staged + unstaged); a repo with no
    // commit yet has no HEAD, so fall back to the plain diffs
    let mut changes: Vec<(u32, u32, String, char)> = Vec::new();
    let tracked = git(&dir, &["diff", "--numstat", "--no-renames", "HEAD"]).or_else(|| {
        let a = git(&dir, &["diff", "--numstat", "--no-renames"]).unwrap_or_default();
        let b = git(&dir, &["diff", "--numstat", "--no-renames", "--cached"]).unwrap_or_default();
        Some(format!("{a}
{b}"))
    });
    for (a, d, path) in parse_numstat(&tracked.unwrap_or_default()) {
        let status = if !dir.join(&path).exists() { 'D' } else { 'M' };
        changes.push((a, d, path, status));
    }
    // brand-new files count as all-added lines
    if let Some(list) = git(&dir, &["ls-files", "--others", "--exclude-standard"]) {
        for path in list.lines().take(MAX_UNTRACKED) {
            let path = path.trim_matches('"').to_string();
            let lines = count_lines(&dir.join(&path)).unwrap_or(0);
            changes.push((lines, 0, path, 'A'));
        }
    }
    info.changed = changes.len();
    info.added = changes.iter().map(|c| c.0).sum();
    info.deleted = changes.iter().map(|c| c.1).sum();
    let mut ranked: Vec<(u64, (u32, u32, String, char))> =
        changes.into_iter().map(|c| (mtime(&dir.join(&c.2)), c)).collect();
    ranked.sort_by(|x, y| y.0.cmp(&x.0));
    info.files = ranked
        .into_iter()
        .take(MAX_FILES)
        .map(|(_, (added, deleted, path, status))| FileChange {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            path,
            added,
            deleted,
            status,
        })
        .collect();

    info
}

pub fn spawn(state: Arc<IslandState>) {
    std::thread::spawn(move || {
        let mut last_name = String::new();
        let mut last_at = std::time::Instant::now() - Duration::from_secs(REFRESH_S);
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            // only while the hub is open (nobody sees it otherwise)
            if !state.hub_open.load(Ordering::Relaxed) {
                continue;
            }
            let name = state.activity.active_project_name();
            if name.is_empty() {
                *state.activity.project.lock().unwrap() = None;
                last_name.clear();
                continue;
            }
            if name == last_name && last_at.elapsed() < Duration::from_secs(REFRESH_S) {
                continue;
            }
            last_name = name.clone();
            last_at = std::time::Instant::now();
            let info = gather(&name);
            *state.activity.project.lock().unwrap() = Some(info);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_parses_text_and_binary() {
        let rows = parse_numstat("12\t3\tsrc/a.rs\n-\t-\timg/x.png\n");
        assert_eq!(rows, vec![(12, 3, "src/a.rs".to_string()), (0, 0, "img/x.png".to_string())]);
    }

    #[test]
    fn gathers_this_repo() {
        let info = gather("not-another-dynamic-island");
        // only meaningful where the editor knows the folder; must never panic
        if info.branch.is_some() {
            assert!(info.files.len() <= MAX_FILES);
            assert!(info.files.len() <= info.changed);
        }
    }
}
