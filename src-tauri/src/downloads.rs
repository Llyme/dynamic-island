//! Download detection: what is being downloaded right now, from the browsers,
//! Steam and qBittorrent. Nothing here talks to those apps -- it watches what
//! they leave on disk:
//!
//! * Chromium browsers (Chrome, Edge, Brave, Vivaldi, Opera): a growing
//!   `.crdownload` file, named and sized through the browser's own history
//!   database (read in place, read-only).
//! * Firefox: a growing `<name>.part` file.
//! * Steam: files growing under `steamapps/downloading`, named from the app's
//!   `appmanifest_*.acf`.
//! * qBittorrent: torrents that are unfinished and not paused (from its
//!   `BT_backup` folder) whose files changed in the last few seconds.
//!
//! Only active downloads are listed. One that finishes stays on the list, marked
//! done, until it is clicked (a browser download then also opens its folder).

use crate::IslandState;
use serde::Serialize;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tauri::{Manager, WebviewWindow};

const POLL_MS: u64 = 1500;
/// a file touched this recently counts as "being downloaded"
const ACTIVE_S: u64 = 15;
/// not seen for this long: check whether it finished
const GONE_S: f64 = 4.0;
/// not seen and not finished for this long: it was cancelled or paused
const DROP_S: f64 = 20.0;
/// still listed, not finished, but nothing moved for this long: it is stuck or paused, so it goes
const STALL_S: f64 = 5.0;

#[derive(Serialize, Clone)]
pub struct DownloadItem {
    pub id: String,
    pub source: String,
    /// "browser", "steam" or "torrent"
    pub kind: String,
    pub name: String,
    pub received: u64,
    pub total: Option<u64>,
    /// bytes per second
    pub speed: f64,
    pub done: bool,
    /// the app behind it, so a click on an active download can bring that app up
    exe_path: Option<String>,
    /// that app's icon (data URL), filled in when the list is handed to the UI
    icon: Option<String>,
    /// what opening a finished browser download reveals
    #[serde(skip)]
    open_path: Option<String>,
    #[serde(skip)]
    meta: String,
    #[serde(skip)]
    last_bytes: u64,
    #[serde(skip)]
    last_at: Instant,
    #[serde(skip)]
    seen: Instant,
    #[serde(skip)]
    started: Instant,
    /// when bytes last came in
    #[serde(skip)]
    moved: Instant,
}

/// what one poll saw for one active download
struct Obs {
    id: String,
    source: String,
    kind: &'static str,
    name: String,
    received: u64,
    total: Option<u64>,
    exe_path: Option<String>,
    open_path: Option<String>,
    meta: String,
    /// bytes per second, when the source knows it (else it is worked out from progress)
    speed: Option<f64>,
}

/// every download, whatever its source, on one card
#[derive(Serialize)]
pub struct DownloadCard {
    /// combined speed of the active ones, bytes per second
    speed: f64,
    items: Vec<DownloadItem>,
}

// ---------------------------------------------------------------------------
// shared helpers

fn secs_since(t: SystemTime) -> u64 {
    SystemTime::now().duration_since(t).map_or(0, |d| d.as_secs())
}

fn recent(t: SystemTime) -> bool {
    secs_since(t) <= ACTIVE_S
}

fn downloads_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join("Downloads"))
}

/// files in `dir` (not recursive) ending in one of `exts`, modified lately: (path, size)
fn partials(dir: &Path, exts: &[&str]) -> Vec<(PathBuf, u64)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension()?.to_string_lossy().to_lowercase();
            if !exts.contains(&ext.as_str()) {
                return None;
            }
            let m = e.metadata().ok()?;
            recent(m.modified().ok()?).then(|| (p, m.len()))
        })
        .collect()
}

fn first_existing(candidates: &[String]) -> Option<String> {
    candidates.iter().find(|p| Path::new(p).exists()).cloned()
}

fn env_path(var: &str, tail: &str) -> String {
    format!("{}\\{}", std::env::var(var).unwrap_or_default(), tail)
}

/// newest modification time, total size and file count under `path` (bounded walk)
fn scan_tree(path: &Path, depth: u32, budget: &mut u32) -> (Option<SystemTime>, u64) {
    let Ok(meta) = std::fs::metadata(path) else { return (None, 0) };
    if meta.is_file() {
        return (meta.modified().ok(), meta.len());
    }
    let mut newest: Option<SystemTime> = None;
    let mut size = 0u64;
    if depth == 0 {
        return (None, 0);
    }
    let Ok(rd) = std::fs::read_dir(path) else { return (None, 0) };
    for e in rd.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let (t, s) = scan_tree(&e.path(), depth - 1, budget);
        size += s;
        newest = newest.max(t);
    }
    (newest, size)
}

// ---------------------------------------------------------------------------
// Chromium browsers

const CHROMIUM: &[(&str, &str)] = &[
    ("Chrome", "chrome.exe"),
    ("Edge", "msedge.exe"),
    ("Brave", "brave.exe"),
    ("Vivaldi", "vivaldi.exe"),
    ("Opera", "opera.exe"),
];

fn browser_exe(stem: &str) -> Option<String> {
    let pf = env_path("ProgramFiles", "");
    let pf86 = env_path("ProgramFiles(x86)", "");
    let local = env_path("LOCALAPPDATA", "");
    let list: Vec<String> = match stem {
        "chrome.exe" => vec![
            format!("{pf}Google\\Chrome\\Application\\chrome.exe"),
            format!("{pf86}Google\\Chrome\\Application\\chrome.exe"),
            format!("{local}Google\\Chrome\\Application\\chrome.exe"),
        ],
        "msedge.exe" => vec![
            format!("{pf86}Microsoft\\Edge\\Application\\msedge.exe"),
            format!("{pf}Microsoft\\Edge\\Application\\msedge.exe"),
        ],
        "brave.exe" => vec![format!("{pf}BraveSoftware\\Brave-Browser\\Application\\brave.exe")],
        "vivaldi.exe" => vec![format!("{local}Vivaldi\\Application\\vivaldi.exe")],
        "opera.exe" => vec![format!("{local}Programs\\Opera\\opera.exe")],
        "firefox.exe" => vec![format!("{pf}Mozilla Firefox\\firefox.exe"), format!("{pf86}Mozilla Firefox\\firefox.exe")],
        _ => vec![],
    };
    first_existing(&list)
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/._~:-".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The browser keeps its history locked, so it is read in place without taking
/// any lock (`immutable`). A read that lands mid-write just fails; the next poll
/// tries again.
fn open_live(path: &Path) -> Option<rusqlite::Connection> {
    let uri = format!("file:///{}?immutable=1", pct_encode(&path.to_string_lossy().replace('\\', "/")));
    rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

fn chromium_active(dirs: &[PathBuf], tracked: bool) -> Vec<Obs> {
    let any_partial = dirs.iter().any(|d| !partials(d, &["crdownload", "opdownload"]).is_empty());
    if !any_partial && !tracked {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (source, stem) in CHROMIUM {
        let Some(hist) = crate::browse::history_path(stem) else { continue };
        let Some(conn) = open_live(&hist) else { continue };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, current_path, target_path, received_bytes, total_bytes FROM downloads WHERE state = 0",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        }) else {
            continue;
        };
        for (id, current, target, received_db, total) in rows.flatten() {
            // a row left in "in progress" by a crashed browser has no growing file
            let Ok(meta) = std::fs::metadata(&current) else { continue };
            if !meta.modified().map_or(false, recent) {
                continue;
            }
            let name = Path::new(&target).file_name().map_or_else(|| "Download".into(), |n| n.to_string_lossy().to_string());
            out.push(Obs {
                id: format!("{stem}:{id}"),
                source: source.to_string(),
                kind: "browser",
                name,
                received: meta.len().max(received_db.max(0) as u64),
                total: (total > 0).then_some(total as u64),
                exe_path: browser_exe(stem),
                open_path: Some(target),
                meta: stem.to_string(),
                speed: None,
            });
        }
    }
    out
}

/// state (1 = complete) and final path of one Chromium download
fn chromium_state(stem: &str, id: i64) -> Option<(i64, String)> {
    let hist = crate::browse::history_path(stem)?;
    let conn = open_live(&hist)?;
    conn.query_row("SELECT state, target_path FROM downloads WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).ok()
}

fn firefox_active(dirs: &[PathBuf]) -> Vec<Obs> {
    let mut out = Vec::new();
    for d in dirs {
        for (part, len) in partials(d, &["part"]) {
            let final_path = part.with_extension("");
            let name = final_path.file_name().map_or_else(|| "Download".into(), |n| n.to_string_lossy().to_string());
            out.push(Obs {
                id: format!("firefox:{}", part.to_string_lossy()),
                source: "Firefox".into(),
                kind: "browser",
                name,
                received: len,
                total: None,
                exe_path: browser_exe("firefox.exe"),
                open_path: Some(final_path.to_string_lossy().to_string()),
                meta: "firefox".into(),
                speed: None,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Steam

fn steam_root() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Valve\Steam").ok()?;
    let p: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(p.replace('/', "\\")))
}

/// `steamapps` folders of every Steam library
fn steam_libraries(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.join("steamapps")];
    if let Ok(text) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('"').collect();
            // \t"path"\t\t"D:\\Games\\Steam"
            if parts.len() >= 4 && parts[1] == "path" {
                let p = PathBuf::from(parts[3].replace("\\\\", "\\")).join("steamapps");
                if !libs.contains(&p) {
                    libs.push(p);
                }
            }
        }
    }
    libs.into_iter().filter(|p| p.is_dir()).collect()
}

/// top-level `"key" "value"` pairs of an appmanifest
fn read_acf(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('"').collect();
            if parts.len() >= 4 {
                map.entry(parts[1].to_string()).or_insert_with(|| parts[3].to_string());
            }
        }
    }
    map
}

/// How fast a process writes to disk, smoothed. A downloading app writes what it
/// receives straight out, so this tracks its download speed -- and it costs
/// nothing to read, unlike walking its files.
#[derive(Default)]
struct IoMeter {
    pid: Option<u32>,
    last: Option<(Instant, u64)>,
    /// bytes per second, smoothed
    rate: f64,
    /// consecutive samples above the caller's threshold
    hits: u32,
}

impl IoMeter {
    fn sample(&mut self, threshold: f64) {
        let now = Instant::now();
        match self.pid.and_then(io_write_bytes) {
            Some(w) => {
                if let Some((t0, w0)) = self.last {
                    let dt = now.duration_since(t0).as_secs_f64();
                    if dt >= 0.5 {
                        let inst = w.saturating_sub(w0) as f64 / dt;
                        self.rate = self.rate * 0.5 + inst * 0.5;
                        self.last = Some((now, w));
                    }
                } else {
                    self.last = Some((now, w));
                }
            }
            None => {
                self.rate = 0.0;
                self.last = None;
            }
        }
        self.hits = if self.rate > threshold { self.hits + 1 } else { 0 };
    }
}

/// Steam keeps no live progress on disk: its manifests only change when a download
/// starts, pauses or finishes. Only the speed can be seen.
#[derive(Default)]
struct SteamMeter {
    io: IoMeter,
    last_call: Option<Instant>,
    /// bytes counted per app since the island first saw it download
    counted: HashMap<String, f64>,
}

/// above this write rate qBittorrent is downloading something (idle, it writes next to nothing)
const QBT_MIN_RATE: f64 = 8_000.0;

/// above this write rate Steam is downloading (idle Steam writes next to nothing)
const STEAM_MIN_RATE: f64 = 300_000.0;

fn io_write_bytes(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetProcessIoCounters, OpenProcess, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut c = IO_COUNTERS::default();
        let ok = GetProcessIoCounters(h, &mut c).is_ok();
        let _ = CloseHandle(h);
        ok.then_some(c.WriteTransferCount)
    }
}

fn steam_active(root: &Path, m: &mut SteamMeter) -> Vec<Obs> {
    let now = Instant::now();
    m.io.sample(STEAM_MIN_RATE);
    let call_dt = m.last_call.map_or(0.0, |t| now.duration_since(t).as_secs_f64()).min(5.0);
    m.last_call = Some(now);
    if m.io.hits < 2 {
        return Vec::new();
    }
    let rate = m.io.rate;

    // which app: one whose manifest says "update running" (flag 1024); if several are
    // queued, the one whose download folder changed last
    let mut best: Option<(SystemTime, String, PathBuf, HashMap<String, String>)> = None;
    for lib in steam_libraries(root) {
        let Ok(rd) = std::fs::read_dir(&lib) else { continue };
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let Some(appid) = file.strip_prefix("appmanifest_").and_then(|r| r.strip_suffix(".acf")) else { continue };
            let acf = read_acf(&e.path());
            let flags = acf.get("StateFlags").and_then(|f| f.parse::<u32>().ok()).unwrap_or(0);
            if flags & 1024 == 0 {
                continue;
            }
            let touched = [lib.join("downloading").join(appid), e.path()]
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if best.as_ref().map_or(true, |(t, ..)| touched > *t) {
                best = Some((touched, appid.to_string(), lib.clone(), acf));
            }
        }
    }
    let Some((_, appid, lib, acf)) = best else { return Vec::new() };
    let total = acf.get("BytesToDownload").and_then(|v| v.parse::<u64>().ok()).filter(|t| *t > 0);
    let base = acf.get("BytesDownloaded").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let counted = m.counted.entry(appid.clone()).or_insert(base);
    *counted += rate * call_dt;
    let received = total.map_or(*counted as u64, |t| (*counted as u64).min(t));
    vec![Obs {
        id: format!("steam:{appid}"),
        source: "Steam".into(),
        kind: "steam",
        name: acf.get("name").cloned().unwrap_or_else(|| format!("App {appid}")),
        received,
        total,
        exe_path: Some(root.join("steam.exe").to_string_lossy().to_string()).filter(|p| Path::new(p).exists()),
        open_path: None,
        meta: format!("{}|{appid}", lib.to_string_lossy()),
        speed: Some(rate),
    }]
}

/// a Steam download is done when its app is installed and no longer updating
fn steam_finished(meta: &str) -> bool {
    let Some((lib, appid)) = meta.rsplit_once('|') else { return false };
    read_acf(&Path::new(lib).join(format!("appmanifest_{appid}.acf")))
        .get("StateFlags")
        .and_then(|f| f.parse::<u32>().ok())
        .map_or(false, |f| f & 4 != 0 && f & 1024 == 0)
}

// ---------------------------------------------------------------------------
// qBittorrent

#[derive(Clone)]
enum B {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<B>),
    Dict(Vec<(Vec<u8>, B)>),
}

fn bparse(b: &[u8], i: &mut usize) -> Option<B> {
    match *b.get(*i)? {
        b'i' => {
            let end = b[*i..].iter().position(|c| *c == b'e')? + *i;
            let n = std::str::from_utf8(&b[*i + 1..end]).ok()?.parse().ok()?;
            *i = end + 1;
            Some(B::Int(n))
        }
        b'l' => {
            *i += 1;
            let mut v = Vec::new();
            while *b.get(*i)? != b'e' {
                v.push(bparse(b, i)?);
            }
            *i += 1;
            Some(B::List(v))
        }
        b'd' => {
            *i += 1;
            let mut v = Vec::new();
            while *b.get(*i)? != b'e' {
                let Some(B::Bytes(k)) = bparse(b, i) else { return None };
                v.push((k, bparse(b, i)?));
            }
            *i += 1;
            Some(B::Dict(v))
        }
        b'0'..=b'9' => {
            let colon = b[*i..].iter().position(|c| *c == b':')? + *i;
            let len: usize = std::str::from_utf8(&b[*i..colon]).ok()?.parse().ok()?;
            let start = colon + 1;
            let end = start.checked_add(len)?;
            if end > b.len() {
                return None;
            }
            *i = end;
            // (only kept up to a size: resume files carry one byte per piece)
            Some(B::Bytes(if len <= 4_000_000 { b[start..end].to_vec() } else { Vec::new() }))
        }
        _ => None,
    }
}

impl B {
    fn get(&self, key: &str) -> Option<&B> {
        match self {
            B::Dict(v) => v.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }
    fn int(&self) -> Option<i64> {
        if let B::Int(n) = self { Some(*n) } else { None }
    }
    fn text(&self) -> Option<String> {
        if let B::Bytes(b) = self { String::from_utf8(b.clone()).ok().filter(|s| !s.is_empty()) } else { None }
    }
}

#[derive(Clone)]
struct Torrent {
    hash: String,
    name: String,
    /// bytes of the files that are wanted
    total: u64,
    content: PathBuf,
    /// qBittorrent's resume file: the only place real progress is written
    resume: PathBuf,
    /// stopped by the user, as of the resume file's last save
    paused: bool,
}

type TorrentCache = HashMap<String, (SystemTime, SystemTime, Option<Torrent>)>;

fn read_bencode(path: &Path) -> Option<B> {
    let bytes = std::fs::read(path).ok()?;
    let mut i = 0;
    bparse(&bytes, &mut i)
}

/// (share of the wanted pieces downloaded, finished?) from a resume file's piece map.
/// qBittorrent preallocates its files, so their size says nothing about progress.
fn resume_progress(r: &B) -> (f64, bool) {
    let finished = r.get("qBt-seedStatus").and_then(B::int).unwrap_or(0) != 0;
    let have = match r.get("pieces") {
        Some(B::Bytes(b)) => b.as_slice(),
        _ => &[],
    };
    if have.is_empty() {
        return (if finished { 1.0 } else { 0.0 }, finished);
    }
    let wanted = match r.get("piece_priority") {
        Some(B::Bytes(b)) => b.as_slice(),
        _ => &[],
    };
    let (mut need, mut got) = (0u64, 0u64);
    for (i, b) in have.iter().enumerate() {
        if wanted.get(i).map_or(false, |p| *p == 0) {
            continue; // in a file the user skipped
        }
        need += 1;
        got += (b & 1) as u64;
    }
    let frac = if need == 0 { 1.0 } else { got as f64 / need as f64 };
    (frac, finished || frac >= 0.9995)
}

/// Torrents that still miss wanted pieces. Parsed once, and again only when their
/// resume file changes (qBittorrent rewrites it when a torrent is paused, resumed or done).
fn refresh_torrents(dir: &Path, cache: &mut TorrentCache) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut alive: Vec<String> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map_or(true, |x| x != "fastresume") {
            continue;
        }
        let Some(hash) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        alive.push(hash.clone());
        let torrent_path = dir.join(format!("{hash}.torrent"));
        let m1 = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
        let m2 = std::fs::metadata(&torrent_path).ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
        if cache.get(&hash).map_or(false, |(a, b, _)| *a == m1 && *b == m2) {
            continue;
        }
        let entry = (|| {
            let resume = read_bencode(&p)?;
            if resume_progress(&resume).1 {
                return None;
            }
            let save = resume.get("qBt-savePath").and_then(B::text).or_else(|| resume.get("save_path").and_then(B::text))?;
            let meta = read_bencode(&torrent_path)?;
            let info = meta.get("info")?;
            let name = info.get("name.utf-8").and_then(B::text).or_else(|| info.get("name").and_then(B::text))?;
            // files the user switched to "do not download" (priority 0) are not part of it
            let priorities: Vec<i64> = match resume.get("file_priority") {
                Some(B::List(v)) => v.iter().map(|p| p.int().unwrap_or(1)).collect(),
                _ => Vec::new(),
            };
            let total = match info.get("length").and_then(B::int) {
                Some(n) => n,
                None => match info.get("files") {
                    Some(B::List(files)) => files
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| priorities.get(*i).map_or(true, |p| *p != 0))
                        .filter_map(|(_, f)| f.get("length").and_then(B::int))
                        .sum(),
                    _ => 0,
                },
            };
            let content = PathBuf::from(save.replace('/', "\\")).join(&name);
            let paused = resume.get("paused").and_then(B::int).unwrap_or(0) != 0;
            Some(Torrent { hash: hash.clone(), name, total: total.max(0) as u64, content, resume: p.clone(), paused })
        })();
        cache.insert(hash, (m1, m2, entry));
    }
    cache.retain(|k, _| alive.contains(k));
}

fn qbittorrent_exe() -> Option<String> {
    first_existing(&[
        env_path("ProgramFiles", "qBittorrent\\qbittorrent.exe"),
        env_path("ProgramFiles(x86)", "qBittorrent\\qbittorrent.exe"),
    ])
}

/// per torrent: the resume file the progress was last read from, how many bytes
/// were estimated on top of it since (the resume file is only saved now and then),
/// and whether its files were being written when last looked at
struct TorrentTrack {
    saved_at: Option<SystemTime>,
    extra: f64,
    last: Instant,
    checked: Instant,
    writing: bool,
}

/// files written this recently count as "writing" (qBittorrent flushes its write
/// cache in bursts, so a slow torrent can go quiet for a while)
const TORRENT_WRITING_S: u64 = 120;
/// walking a torrent's files is the costly part: not more often than this
const TORRENT_CHECK_S: f64 = 3.0;

/// Every torrent that is unfinished and not paused: what qBittorrent lists as
/// downloading, stalled or queued. Progress comes from the resume file; the ones
/// whose files are being written also get the client's write rate (shared between
/// them) as their speed, and their progress is carried forward with it until the
/// next save corrects it.
fn torrents_active(cache: &TorrentCache, track: &mut HashMap<String, TorrentTrack>, rate: f64) -> Vec<Obs> {
    let exe = qbittorrent_exe();
    let now = Instant::now();
    track.retain(|k, _| cache.contains_key(k));
    let mut listed: Vec<&Torrent> = Vec::new();
    for t in cache.values().filter_map(|(_, _, t)| t.as_ref()).filter(|t| !t.paused) {
        let tr = track.entry(t.hash.clone()).or_insert(TorrentTrack {
            saved_at: None,
            extra: 0.0,
            last: now,
            checked: now - Duration::from_secs(60),
            writing: false,
        });
        if now.duration_since(tr.checked).as_secs_f64() >= TORRENT_CHECK_S {
            let mut budget = 2000;
            let (newest, _) = scan_tree(&t.content, 5, &mut budget);
            tr.writing = newest.map_or(false, |m| secs_since(m) <= TORRENT_WRITING_S);
            tr.checked = now;
        }
        listed.push(t);
    }
    let writing = listed.iter().filter(|t| track.get(&t.hash).map_or(false, |tr| tr.writing)).count();
    let share = if writing == 0 { 0.0 } else { rate / writing as f64 };
    let mut out = Vec::new();
    for t in listed {
        let Some(r) = read_bencode(&t.resume) else { continue };
        let saved = std::fs::metadata(&t.resume).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let frac = resume_progress(&r).0;
        let tr = track.get_mut(&t.hash).unwrap();
        if tr.saved_at != Some(saved) {
            tr.saved_at = Some(saved);
            tr.extra = 0.0; // a fresh save: the resume file is right again
        }
        let dt = now.duration_since(tr.last).as_secs_f64().min(5.0);
        tr.last = now;
        let speed = if tr.writing { share } else { 0.0 };
        tr.extra += speed * dt;
        let total = t.total as f64;
        let received = if t.total > 0 { (frac * total + tr.extra).min(total * 0.999) } else { 0.0 };
        out.push(Obs {
            id: format!("qbt:{}", t.hash),
            source: "qBittorrent".into(),
            kind: "torrent",
            name: t.name.clone(),
            received: received as u64,
            total: (t.total > 0).then_some(t.total),
            exe_path: exe.clone(),
            open_path: Some(t.content.to_string_lossy().to_string()),
            meta: t.resume.to_string_lossy().to_string(),
            speed: Some(speed),
        });
    }
    out
}

/// qBittorrent saves the resume file the moment a torrent completes
fn torrent_finished(meta: &str) -> bool {
    read_bencode(Path::new(meta)).map_or(false, |r| resume_progress(&r).1)
}

/// pid of the process with exactly this executable name
fn process_pid(sys: &sysinfo::System, name: &str) -> Option<u32> {
    sys.processes_by_exact_name(std::ffi::OsStr::new(name)).next().map(|p| p.pid().as_u32())
}

fn qbittorrent_running(sys: &mut sysinfo::System) -> bool {
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let found = sys.processes_by_name(std::ffi::OsStr::new("qbittorrent")).next().is_some();
    found
}

// ---------------------------------------------------------------------------
// the poll loop

/// downloads that were dropped for standing still: id -> the bytes they had. One that is
/// still stuck is not listed again; it comes back when it moves.
fn stalled() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static S: std::sync::OnceLock<std::sync::Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn merge(items: &mut Vec<DownloadItem>, seen: Vec<Obs>) {
    let now = Instant::now();
    for o in seen {
        match items.iter_mut().find(|i| i.id == o.id && !i.done) {
            Some(it) => {
                if o.received > it.received || o.speed.map_or(false, |s| s > 0.0) {
                    it.moved = now;
                }
                let dt = now.duration_since(it.last_at).as_secs_f64();
                if dt >= 0.5 {
                    let inst = o.received.saturating_sub(it.last_bytes) as f64 / dt;
                    it.speed = if it.speed == 0.0 { inst } else { it.speed * 0.6 + inst * 0.4 };
                    it.last_bytes = o.received;
                    it.last_at = now;
                }
                if let Some(s) = o.speed {
                    it.speed = s;
                }
                it.received = o.received;
                it.total = o.total;
                it.name = o.name;
                it.seen = now;
            }
            None => {
                // a finished one that is still on the list is not re-added
                if items.iter().any(|i| i.id == o.id) {
                    continue;
                }
                // one that was dropped for standing still stays off the list until it moves
                {
                    let mut st = stalled().lock().unwrap();
                    match st.get(&o.id) {
                        Some(&bytes) if o.received <= bytes && !o.speed.map_or(false, |s| s > 0.0) => continue,
                        Some(_) => {
                            st.remove(&o.id);
                        }
                        None => {}
                    }
                }
                items.push(DownloadItem {
                    id: o.id,
                    source: o.source,
                    kind: o.kind.to_string(),
                    name: o.name,
                    received: o.received,
                    total: o.total,
                    speed: 0.0,
                    done: false,
                    exe_path: o.exe_path,
                    icon: None,
                    open_path: o.open_path,
                    meta: o.meta,
                    last_bytes: o.received,
                    last_at: now,
                    seen: now,
                    started: now,
                    moved: now,
                });
            }
        }
    }
    // downloads that stopped showing up: finished, or dropped
    for it in items.iter_mut().filter(|i| !i.done) {
        let gone = now.duration_since(it.seen).as_secs_f64();
        if gone < GONE_S {
            continue;
        }
        let finished = match it.kind.as_str() {
            "steam" => steam_finished(&it.meta),
            "torrent" => torrent_finished(&it.meta),
            _ if it.meta == "firefox" => {
                let fin = it.open_path.as_deref().map_or(false, |p| Path::new(p).exists());
                let part_left = it.id.strip_prefix("firefox:").map_or(false, |p| Path::new(p).exists());
                fin && !part_left
            }
            _ => match it.id.split_once(':').and_then(|(stem, id)| Some((stem.to_string(), id.parse::<i64>().ok()?))) {
                Some((stem, id)) => match chromium_state(&stem, id) {
                    Some((1, target)) => {
                        it.open_path = Some(target);
                        true
                    }
                    _ => false,
                },
                None => false,
            },
        };
        if finished {
            it.done = true;
            it.speed = 0.0;
            if let Some(t) = it.total {
                it.received = t;
            }
        }
    }
    items.retain(|i| i.done || now.duration_since(i.seen).as_secs_f64() < DROP_S);
    // listed and not finished, but nothing has moved for a while: stuck or paused
    let mut st = stalled().lock().unwrap();
    if st.len() > 64 {
        st.clear();
    }
    items.retain(|i| {
        let stuck = !i.done && now.duration_since(i.moved).as_secs_f64() > STALL_S;
        if stuck {
            st.insert(i.id.clone(), i.received);
        }
        !stuck
    });
}

pub fn spawn(state: Arc<IslandState>) {
    std::thread::spawn(move || {
        // background work: never compete with the UI
        unsafe {
            let _ = windows::Win32::System::Threading::SetThreadPriority(
                windows::Win32::System::Threading::GetCurrentThread(),
                windows::Win32::System::Threading::THREAD_PRIORITY_BELOW_NORMAL,
            );
        }
        let mut sys = sysinfo::System::new();
        let mut steam_meter = SteamMeter::default();
        let mut qbt_meter = IoMeter::default();
        let mut qbt_running = false;
        let mut qbt_checked = Instant::now() - Duration::from_secs(60);
        let mut torrents: TorrentCache = HashMap::new();
        let mut torrent_track: HashMap<String, TorrentTrack> = HashMap::new();
        let mut torrents_at = Instant::now() - Duration::from_secs(60);
        // debug aid: pretend a few downloads exist (`DI_DEMO_DOWNLOADS=1`) to look at the cards
        let demo = std::env::var_os("DI_DEMO_DOWNLOADS").is_some();
        let demo_start = Instant::now();
        let open_hub = std::env::var_os("DI_OPEN_HUB").is_some();
        let bt_dir = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("qBittorrent").join("BT_backup"));
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            if !state.settings.lock().unwrap().download_detection {
                state.activity.downloads.lock().unwrap().clear();
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
            if open_hub && demo_start.elapsed().as_secs() == 3 && !state.hub_open.load(Ordering::Relaxed) {
                state.hub_open.store(true, Ordering::Relaxed);
                state.shown.store(true, Ordering::Relaxed);
            }
            let dirs: Vec<PathBuf> = downloads_dir().into_iter().collect();
            let tracked_browser = state.activity.downloads.lock().unwrap().iter().any(|i| i.kind == "browser" && !i.done);

            let mut seen: Vec<Obs> = Vec::new();
            seen.extend(chromium_active(&dirs, tracked_browser));
            seen.extend(firefox_active(&dirs));
            if qbt_checked.elapsed() > Duration::from_secs(10) {
                qbt_running = qbittorrent_running(&mut sys);
                steam_meter.io.pid = process_pid(&sys, "steam.exe");
                qbt_meter.pid = process_pid(&sys, "qbittorrent.exe");
                qbt_checked = Instant::now();
            }
            if let Some(root) = steam_root() {
                seen.extend(steam_active(&root, &mut steam_meter));
            }
            if qbt_running {
                if torrents_at.elapsed() > Duration::from_secs(20) {
                    if let Some(dir) = &bt_dir {
                        refresh_torrents(dir, &mut torrents);
                    }
                    torrents_at = Instant::now();
                }
                qbt_meter.sample(QBT_MIN_RATE);
                seen.extend(torrents_active(&torrents, &mut torrent_track, qbt_meter.rate));
            }
            if demo {
                let t = demo_start.elapsed().as_secs_f64();
                for (i, (name, total)) in [("Neverness to Everness", 24_000_000_000u64), ("Deep Rock Galactic", 3_100_000_000), ("Hades II", 9_800_000_000), ("Balatro", 90_000_000)].iter().enumerate() {
                    seen.push(Obs {
                        id: format!("steam:demo{i}"),
                        source: "Steam".into(),
                        kind: "steam",
                        name: name.to_string(),
                        received: ((t * (20_000_000.0 + i as f64 * 5_000_000.0)) as u64).min(*total - 1),
                        total: Some(*total),
                        exe_path: None,
                        open_path: None,
                        meta: String::new(),
                        speed: None,
                    });
                }
                seen.push(Obs {
                    id: "demo:chrome".into(),
                    source: "Chrome".into(),
                    kind: "browser",
                    name: "installer-x64.exe".into(),
                    received: (t * 4_000_000.0) as u64,
                    total: None,
                    exe_path: None,
                    open_path: None,
                    meta: "demo".into(),
                    speed: None,
                });
                if t > 3.0 && !state.hub_open.load(Ordering::Relaxed) && t < 4.0 {
                    // demo mode also opens the hub, so the cards can be looked at
                    state.hub_open.store(true, Ordering::Relaxed);
                    state.shown.store(true, Ordering::Relaxed);
                }
                let mut items = state.activity.downloads.lock().unwrap();
                if t > 8.0 && !items.iter().any(|i| i.id == "demo:done") {
                    let now = Instant::now();
                    items.push(DownloadItem {
                        id: "demo:done".into(), source: "Chrome".into(), kind: "browser".into(), name: "report-final.pdf".into(),
                        received: 1, total: Some(1), speed: 0.0, done: true, exe_path: None, icon: None, open_path: None, meta: String::new(),
                        last_bytes: 0, last_at: now, seen: now, started: now, moved: now,
                    });
                }
                merge(&mut items, seen);
                continue;
            }
            merge(&mut state.activity.downloads.lock().unwrap(), seen);
        }
    });
}

/// The list as one hub card: active downloads first (oldest first), finished ones after.
pub fn cards(items: &[DownloadItem]) -> Vec<DownloadCard> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut all: Vec<DownloadItem> = items.to_vec();
    for i in all.iter_mut() {
        i.icon = i.exe_path.as_deref().and_then(|p| crate::exeinfo::lookup(p).icon);
    }
    all.sort_by_key(|i| (i.done, i.started));
    vec![DownloadCard { speed: all.iter().filter(|i| !i.done).map(|i| i.speed).sum(), items: all }]
}

/// A click on a finished download: a browser one also opens its folder (with the
/// file selected); every finished one is dismissed. Active ones are left alone.
#[tauri::command]
pub fn download_item_click(window: WebviewWindow, id: String) -> bool {
    let state = window.state::<Arc<IslandState>>();
    let mut items = state.activity.downloads.lock().unwrap();
    let Some(pos) = items.iter().position(|i| i.id == id && i.done) else { return false };
    let item = items.remove(pos);
    if item.kind == "browser" {
        if let Some(path) = item.open_path {
            let _ = std::process::Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{path}\""))
                .creation_flags(0x0800_0000)
                .spawn();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_reads_the_bits_we_need() {
        let data = b"d4:infod6:lengthi1234e4:name5:a.zipe9:save_path3:C:/e";
        let mut i = 0;
        let v = bparse(data, &mut i).unwrap();
        assert_eq!(v.get("info").unwrap().get("length").and_then(B::int), Some(1234));
        assert_eq!(v.get("info").unwrap().get("name").and_then(B::text).as_deref(), Some("a.zip"));
        assert_eq!(v.get("save_path").and_then(B::text).as_deref(), Some("C:/"));
    }

    /// Manual: runs every detector once against this machine; prints counts, not file names.
    /// `cargo test --release live_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_probe() {
        let dirs: Vec<PathBuf> = downloads_dir().into_iter().collect();
        println!("chromium active: {}", chromium_active(&dirs, true).len());
        for (src, stem) in CHROMIUM {
            let ok = crate::browse::history_path(stem).and_then(|h| open_live(&h)).map(|c| {
                c.query_row("SELECT COUNT(*) FROM downloads", [], |r| r.get::<_, i64>(0)).unwrap_or(-1)
            });
            println!("{src}: history readable={} downloads rows={:?}", ok.is_some(), ok);
        }
        println!("firefox active: {}", firefox_active(&dirs).len());
        let mut cache = TorrentCache::new();
        if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("qBittorrent").join("BT_backup")) {
            refresh_torrents(&dir, &mut cache);
        }
        let candidates = cache.values().filter(|(_, _, t)| t.is_some()).count();
        println!("torrents parsed={} unfinished+unpaused={} active now={}", cache.len(), candidates, torrents_active(&cache, &mut HashMap::new(), 0.0).len());
    }

    #[test]
    fn live_uri_is_encoded() {
        assert_eq!(pct_encode("C:/Users/A B/History"), "C:/Users/A%20B/History");
    }

    #[test]
    fn all_downloads_share_one_card() {
        let mk = |id: &str, src: &str, done: bool| DownloadItem {
            id: id.into(),
            source: src.into(),
            kind: "steam".into(),
            name: id.into(),
            received: 0,
            total: None,
            speed: 10.0,
            done,
            exe_path: None,
            icon: None,
            open_path: None,
            meta: String::new(),
            last_bytes: 0,
            last_at: Instant::now(),
            seen: Instant::now(),
            started: Instant::now(),
            moved: Instant::now(),
        };
        let items = vec![mk("a", "Steam", true), mk("b", "Steam", false), mk("c", "Chrome", false), mk("d", "qBittorrent", false), mk("e", "Steam", false)];
        let cards = cards(&items);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].items.len(), 5);
        assert!(cards[0].items.last().unwrap().done); // finished ones sink to the bottom
        assert_eq!(cards[0].speed, 40.0);
    }
}
