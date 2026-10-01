//! LLM Detection: the Claude Code sessions running on this machine, as cards.
//!
//! Claude Code keeps a small registry file per running session in `~/.claude/sessions/<pid>.json`
//! (process id, session id, folder, where it runs, busy/idle/waiting) and the conversation itself in
//! `~/.claude/projects/<folder>/<session id>.jsonl`. Both are read here, only while the hub shows
//! the cards (the caller is `get_activity`, about once a second): the registry is a handful of tiny
//! files, and a transcript is only parsed again when it changed.

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// a session that went idle this recently, after doing work, counts as finished (its result waits for you)
const FINISHED_S: u64 = 15 * 60;
/// how much of the end of a transcript is read: enough for the latest turns, not the whole history
const TAIL_BYTES: u64 = 400_000;

#[derive(Serialize, Clone)]
pub struct FeedItem {
    icon: &'static str,
    text: String,
}

#[derive(Serialize, Clone)]
pub struct LlmCard {
    id: String,
    pid: u32,
    provider: &'static str,
    /// the session's title (what the conversation is about), else its folder
    title: String,
    project: String,
    /// where it runs: "VS Code", "Terminal", ...
    host: String,
    host_icon: &'static str,
    host_exe: Option<String>,
    /// waiting | finished | working | idle
    state: &'static str,
    activity: FeedItem,
    context_tokens: u64,
    context_limit: u64,
    model: String,
    running_secs: u64,
    prompt: String,
    feed: Vec<FeedItem>,
    cost_usd: f64,
    lines_added: u64,
    lines_removed: u64,
}

#[derive(Clone, Default)]
struct Parsed {
    title: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
    context_tokens: u64,
    cost_usd: f64,
    lines_added: u64,
    lines_removed: u64,
    /// the newest tool calls, newest first
    feed: Vec<FeedItem>,
    /// what the model is doing at the end of the transcript
    last: Option<FeedItem>,
}

#[derive(Clone)]
struct Host {
    label: String,
    icon: &'static str,
    exe: Option<String>,
}

struct Cache {
    parsed: HashMap<PathBuf, (SystemTime, u64, Parsed)>,
    paths: HashMap<String, (PathBuf, Instant)>,
    hosts: HashMap<String, Host>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Cache { parsed: HashMap::new(), paths: HashMap::new(), hosts: HashMap::new() }))
}

/// where Claude Code keeps its files: this Windows account, and each running WSL distro
#[derive(Clone)]
struct Root {
    /// the `.claude` folder
    dir: PathBuf,
    /// the distro and its file system as Windows sees it (`\\wsl.localhost\<distro>`); None = Windows
    wsl: Option<(String, PathBuf)>,
}

struct Reg {
    root: Root,
    reg: Value,
}

#[derive(Default)]
struct WslCache {
    at: Option<std::time::Instant>,
    roots: Vec<Root>,
    busy: bool,
}

/// The `.claude` folders of the WSL distros that are running now. Reading a stopped distro's file
/// system would start it, so only running ones are looked at. The list is found by a short
/// `wsl.exe` call on a background thread (at most every 15 s); until it answers, there are none.
fn wsl_roots() -> Vec<Root> {
    static W: OnceLock<Mutex<WslCache>> = OnceLock::new();
    let m = W.get_or_init(|| Mutex::new(WslCache::default()));
    let mut w = m.lock().unwrap();
    let stale = w.at.map_or(true, |t| t.elapsed() > Duration::from_secs(15));
    if stale && !w.busy {
        w.busy = true;
        std::thread::spawn(|| {
            let roots = scan_wsl();
            let mut w = W.get().unwrap().lock().unwrap();
            w.roots = roots;
            w.at = Some(std::time::Instant::now());
            w.busy = false;
        });
    }
    w.roots.clone()
}

/// `wsl.exe` prints UTF-16 when it writes to a pipe
fn decode_wsl_output(b: &[u8]) -> String {
    if b.contains(&0) {
        let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

fn running_distros(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim_matches(|c: char| c == '\0' || c == '\u{feff}' || c.is_whitespace()))
        // the helper distros of Docker Desktop hold no sessions; a sentence is a message, not a name
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.starts_with("docker-desktop"))
        .map(str::to_string)
        .collect()
}

fn scan_wsl() -> Vec<Root> {
    use std::os::windows::process::CommandExt;
    let Some(win) = std::env::var_os("SystemRoot") else { return Vec::new() };
    let exe = PathBuf::from(win).join("System32").join("wsl.exe");
    if !exe.is_file() {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new(exe).args(["-l", "--running", "-q"]).creation_flags(0x0800_0000).output() else { return Vec::new() };
    let mut roots = Vec::new();
    for name in running_distros(&decode_wsl_output(&out.stdout)) {
        let Some(base) = [format!(r"\\wsl.localhost\{name}"), format!(r"\\wsl$\{name}")].into_iter().map(PathBuf::from).find(|b| b.join("home").is_dir() || b.join("root").is_dir()) else { continue };
        let mut homes = vec![base.join("root")];
        if let Ok(rd) = std::fs::read_dir(base.join("home")) {
            homes.extend(rd.flatten().map(|e| e.path()));
        }
        for h in homes {
            let dir = h.join(".claude");
            if dir.join("sessions").is_dir() {
                roots.push(Root { dir, wsl: Some((name.clone(), base.clone())) });
            }
        }
    }
    roots
}

fn roots() -> Vec<Root> {
    let mut v: Vec<Root> = claude_dir().map(|dir| Root { dir, wsl: None }).into_iter().collect();
    v.extend(wsl_roots());
    v
}

/// is the process of a session still there? In WSL it is the distro's /proc that knows (a Linux
/// process id means nothing to Windows); when /proc cannot be read, a session that changed
/// within the last few hours counts.
fn session_alive(root: &Root, pid: u32, reg: &Value) -> bool {
    let Some((_, base)) = &root.wsl else { return crate::game::pid_alive(pid) };
    let proc = base.join("proc");
    if proc.join(pid.to_string()).exists() {
        return true;
    }
    if proc.join("1").exists() {
        return false;
    }
    let updated = reg.get("statusUpdatedAt").or_else(|| reg.get("startedAt")).and_then(Value::as_u64).unwrap_or(0);
    now_ms().saturating_sub(updated) < 6 * 3600 * 1000
}

/// the registry file of every Claude Code session on this machine (and in its running WSL distros)
fn registries(alive_only: bool) -> Vec<Reg> {
    let mut out = Vec::new();
    for root in roots() {
        let Ok(rd) = std::fs::read_dir(root.dir.join("sessions")) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().map_or(true, |x| x != "json") {
                continue;
            }
            let Some(reg) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
            let Some(pid) = reg.get("pid").and_then(Value::as_u64) else { continue };
            if alive_only && !session_alive(&root, pid as u32, &reg) {
                continue;
            }
            out.push(Reg { root: root.clone(), reg });
        }
    }
    out
}

fn claude_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".claude"))
}

fn basename(p: &str) -> String {
    p.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(p).to_string()
}

fn short(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s
    } else {
        format!("{}\u{2026}", s.chars().take(n).collect::<String>())
    }
}

/// a tool call as one line with an icon: what the model is doing right now
fn tool_item(name: &str, input: &Value) -> FeedItem {
    let file = |key: &str| input.get(key).and_then(Value::as_str).map(basename).unwrap_or_default();
    match name {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => FeedItem { icon: "pen", text: format!("Editing {}", file("file_path")) },
        "Read" => FeedItem { icon: "eye", text: format!("Reading {}", file("file_path")) },
        "Bash" | "PowerShell" => {
            let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
            FeedItem { icon: "term", text: format!("Running {}", short(cmd, 38)) }
        }
        "Grep" | "Glob" => FeedItem { icon: "search", text: "Searching the code".into() },
        "Task" | "Agent" => FeedItem { icon: "sparkle", text: "Running an agent".into() },
        "WebFetch" | "WebSearch" => FeedItem { icon: "globe", text: "Searching the web".into() },
        "TodoWrite" | "ExitPlanMode" => FeedItem { icon: "sparkle", text: "Planning".into() },
        n if n.starts_with("mcp__") => FeedItem { icon: "sparkle", text: format!("Using {}", n.rsplit("__").next().unwrap_or(n)) },
        n => FeedItem { icon: "sparkle", text: n.to_string() },
    }
}

/// the text of a user line that is something the person typed (not a tool result, a system
/// message or a task notification)
fn human_prompt(v: &Value) -> Option<String> {
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(kind) = v.pointer("/origin/kind").and_then(Value::as_str) {
        if kind != "human" {
            return None;
        }
    }
    let content = v.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let t = text.trim();
    // injected context (<system-reminder>, <command-name> ...) is not a prompt
    (!t.is_empty() && !t.starts_with('<')).then(|| t.to_string())
}

/// a finished session the person has looked at (its card was clicked): session id -> the
/// `statusUpdatedAt` it was dismissed at. It comes back only if the session finishes again.
fn dismissed() -> &'static Mutex<HashMap<String, u64>> {
    static D: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(HashMap::new()))
}

/// the person looked at a finished session: its card goes away until it finishes something new
#[tauri::command]
pub fn llm_dismiss(id: String) {
    for Reg { reg, .. } in registries(false) {
        if reg.get("sessionId").and_then(Value::as_str) == Some(id.as_str()) {
            let started = reg.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
            let updated = reg.get("statusUpdatedAt").and_then(Value::as_u64).unwrap_or(started);
            dismissed().lock().unwrap().insert(id, updated);
            return;
        }
    }
}

fn read_tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // a tail starts mid-line: drop the cut-off first line
    Some(if start > 0 { text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default() } else { text })
}

fn parse_transcript(text: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut calls: Vec<FeedItem> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => p.title = v.get("aiTitle").and_then(Value::as_str).map(str::to_string).or(p.title.take()),
            Some("cost-state") => {
                p.cost_usd = v.get("totalCostUSD").and_then(Value::as_f64).unwrap_or(p.cost_usd);
                p.lines_added = v.get("totalLinesAdded").and_then(Value::as_u64).unwrap_or(p.lines_added);
                p.lines_removed = v.get("totalLinesRemoved").and_then(Value::as_u64).unwrap_or(p.lines_removed);
            }
            Some("assistant") if v.get("isSidechain").and_then(Value::as_bool) != Some(true) => {
                let Some(m) = v.get("message") else { continue };
                if let Some(model) = m.get("model").and_then(Value::as_str) {
                    p.model = Some(model.to_string());
                }
                if let Some(u) = m.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let total = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                    if total > 0 {
                        p.context_tokens = total;
                    }
                }
                if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("tool_use") => {
                                let item = tool_item(b.get("name").and_then(Value::as_str).unwrap_or("tool"), b.get("input").unwrap_or(&Value::Null));
                                calls.push(item.clone());
                                p.last = Some(item);
                            }
                            Some("thinking") => p.last = Some(FeedItem { icon: "sparkle", text: "Thinking".into() }),
                            Some("text") => p.last = Some(FeedItem { icon: "sparkle", text: "Replying".into() }),
                            _ => {}
                        }
                    }
                }
            }
            // a tool result arrived: the model is working on it
            Some("user") if v.get("isSidechain").and_then(Value::as_bool) != Some(true) => {
                if v.get("toolUseResult").is_some() {
                    p.last = Some(FeedItem { icon: "sparkle", text: "Thinking".into() });
                } else if let Some(text) = human_prompt(&v) {
                    // the prompt the model is answering: a queued one only lands here once it is dequeued
                    p.prompt = Some(short(&text, 220));
                }
            }
            _ => {}
        }
    }
    p.feed = calls.into_iter().rev().take(3).collect();
    p
}

fn transcript_path(session_id: &str, claude: &Path, registry: &Value) -> Option<PathBuf> {
    if let Some(p) = registry.get("logPath").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut c = cache().lock().unwrap();
    if let Some((p, _)) = c.paths.get(session_id).filter(|(p, at)| p.is_file() && at.elapsed().as_secs() < 20) {
        return Some(p.clone());
    }
    // the same session can have a transcript in more than one project folder (a renamed or
    // moved project keeps the old copy): the one written last is the live one
    let name = format!("{session_id}.jsonl");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in std::fs::read_dir(claude.join("projects")).ok()?.flatten() {
        let p = dir.path().join(&name);
        if let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) {
            if best.as_ref().map_or(true, |(t, _)| m > *t) {
                best = Some((m, p));
            }
        }
    }
    let (_, p) = best?;
    c.paths.insert(session_id.to_string(), (p.clone(), Instant::now()));
    Some(p)
}

fn parsed(path: &Path) -> Parsed {
    let Ok(meta) = std::fs::metadata(path) else { return Parsed::default() };
    let (mtime, len) = (meta.modified().unwrap_or(UNIX_EPOCH), meta.len());
    if let Some((m, l, p)) = cache().lock().unwrap().parsed.get(path) {
        if *m == mtime && *l == len {
            return p.clone();
        }
    }
    let p = read_tail(path).map(|t| parse_transcript(&t)).unwrap_or_default();
    cache().lock().unwrap().parsed.insert(path.to_path_buf(), (mtime, len, p.clone()));
    p
}

/// the window a session runs in: VS Code, Cursor, Windows Terminal... found by walking up the
/// process tree once per session (cached)
fn host_of(root: &Root, pid: u32, entrypoint: &str) -> Host {
    let key = match &root.wsl {
        Some((distro, _)) => format!("wsl:{distro}:{pid}"),
        None => pid.to_string(),
    };
    if let Some(h) = cache().lock().unwrap().hosts.get(&key) {
        return h.clone();
    }
    let (label, icon) = match entrypoint {
        "claude-vscode" => ("VS Code", "code"),
        "cli" => ("Terminal", "term"),
        e if e.contains("desktop") => ("Claude Desktop", "sparkle"),
        _ => ("Claude Code", "sparkle"),
    };
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new().with_exe(UpdateKind::OnlyIfNotSet));
    let mut exe: Option<String> = None;
    if root.wsl.is_some() {
        // the process tree is Linux's, out of sight: the window is whichever editor or terminal
        // of that kind is running on Windows (the window is then picked by the project's name)
        let want: &[&str] = if icon == "code" {
            &["code.exe", "cursor.exe", "code - insiders.exe", "windsurf.exe"]
        } else {
            &["windowsterminal.exe", "wezterm-gui.exe", "alacritty.exe"]
        };
        exe = sys
            .processes()
            .values()
            .find(|p| want.contains(&p.name().to_string_lossy().to_lowercase().as_str()))
            .and_then(|p| p.exe().map(|e| e.to_string_lossy().to_string()));
    } else {
        let mut cur = sysinfo::Pid::from_u32(pid);
        for _ in 0..8 {
            let Some(parent) = sys.process(cur).and_then(|p| p.parent()) else { break };
            let Some(pp) = sys.process(parent) else { break };
            let name = pp.name().to_string_lossy().to_lowercase();
            if ["code.exe", "cursor.exe", "windowsterminal.exe", "code - insiders.exe", "windsurf.exe"].contains(&name.as_str()) {
                exe = pp.exe().map(|p| p.to_string_lossy().to_string());
                // keep walking: the topmost window process is the one with the windows
                cur = parent;
                continue;
            }
            if exe.is_none() && ["pwsh.exe", "powershell.exe", "cmd.exe"].contains(&name.as_str()) {
                exe = pp.exe().map(|p| p.to_string_lossy().to_string());
            }
            cur = parent;
        }
    }
    let host = Host { label: label.to_string(), icon, exe };
    cache().lock().unwrap().hosts.insert(key, host.clone());
    host
}

fn context_limit(model: &str) -> u64 {
    let m = model.to_lowercase();
    if m.contains("haiku") {
        200_000
    } else {
        1_000_000
    }
}

/// "claude-sonnet-5-5" -> "Sonnet 5.5"; "claude-haiku-4-5-20251001" -> "Haiku 4.5"
fn model_name(model: &str) -> String {
    let rest = model.trim_start_matches("claude-");
    let mut parts = rest.split('-');
    let Some(family) = parts.next().filter(|f| !f.is_empty()) else { return String::new() };
    let mut name: String = family.chars().take(1).flat_map(char::to_uppercase).chain(family.chars().skip(1)).collect();
    let version: Vec<&str> = parts.filter(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())).collect();
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// every running Claude Code session: the ones waiting for you first, then working, then idle
pub fn cards() -> Vec<LlmCard> {
    let mut out: Vec<(u64, LlmCard)> = Vec::new();
    for Reg { root, reg } in registries(true) {
        let claude = root.dir.clone();
        let (Some(pid), Some(id)) = (reg.get("pid").and_then(Value::as_u64), reg.get("sessionId").and_then(Value::as_str)) else { continue };
        let pid = pid as u32;
        let str_of = |k: &str| reg.get(k).and_then(Value::as_str).unwrap_or("");
        if !matches!(str_of("kind"), "interactive" | "") {
            continue;
        }
        let t = transcript_path(id, &claude, &reg).map(|p| parsed(&p)).unwrap_or_default();
        let status = str_of("status");
        let waiting_for = reg.get("waitingFor").and_then(Value::as_str).filter(|s| !s.is_empty());
        let started = reg.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
        let updated = reg.get("statusUpdatedAt").and_then(Value::as_u64).unwrap_or(started);
        let state = if waiting_for.is_some() || status.contains("wait") {
            "waiting"
        } else if status == "busy" {
            "working"
        } else if t.model.is_some()
            && now_ms().saturating_sub(updated) / 1000 <= FINISHED_S
            && dismissed().lock().unwrap().get(id) != Some(&updated)
        {
            // idle again after a turn, a short while ago: done, and the result is yours to look at
            "finished"
        } else {
            "idle"
        };
        let activity = match state {
            "waiting" => FeedItem {
                icon: "phone",
                text: match waiting_for.map(str::to_lowercase) {
                    Some(w) if w.contains("perm") || w.contains("approv") => "Waiting for your approval".into(),
                    Some(w) if w.contains("input") || w.contains("answer") || w.contains("question") => "Waiting for your answer".into(),
                    _ => "Waiting for you".into(),
                },
            },
            "working" => t.last.clone().unwrap_or(FeedItem { icon: "sparkle", text: "Thinking".into() }),
            "finished" => FeedItem { icon: "check", text: "Finished".into() },
            _ => FeedItem { icon: "moon", text: "Idle".into() },
        };
        // idle sessions are not shown
        if state == "idle" {
            continue;
        }
        let cwd = str_of("cwd");
        let project = basename(cwd);
        let host = host_of(&root, pid, str_of("entrypoint"));
        let model = t.model.clone().unwrap_or_default();
        // needs you > finished > the others (working, then idle); the most recent first within each
        let rank = match state {
            "waiting" => 0,
            "finished" => 1,
            "working" => 2,
            _ => 3,
        };
        out.push((
            rank * 1_000_000_000_000_000 + (u64::MAX / 4).saturating_sub(updated).min(999_999_999_999_999),
            LlmCard {
                id: id.to_string(),
                pid,
                provider: "claude",
                title: t.title.clone().filter(|s| !s.is_empty()).or_else(|| reg.get("name").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| project.clone()),
                project,
                host: host.label,
                host_icon: host.icon,
                host_exe: host.exe,
                state,
                activity,
                context_tokens: t.context_tokens,
                context_limit: context_limit(&model),
                model: model_name(&model),
                running_secs: now_ms().saturating_sub(started) / 1000,
                prompt: t.prompt.unwrap_or_default(),
                feed: t.feed,
                cost_usd: t.cost_usd,
                lines_added: t.lines_added,
                lines_removed: t.lines_removed,
            },
        ));
    }
    out.sort_by_key(|(k, _)| *k);
    out.into_iter().map(|(_, c)| c).collect()
}

// ---- brief-show: the island drops down when a session needs you or finishes ----

/// a session, as far as its registry file says (no transcript is read until something happens)
struct Live {
    id: String,
    /// waiting | busy | idle
    kind: &'static str,
    updated: u64,
}

fn live_sessions() -> Vec<Live> {
    let mut best: HashMap<String, Live> = HashMap::new();
    for Reg { reg, .. } in registries(true) {
        let Some(id) = reg.get("sessionId").and_then(Value::as_str) else { continue };
        if !matches!(reg.get("kind").and_then(Value::as_str).unwrap_or(""), "interactive" | "") {
            continue;
        }
        let status = reg.get("status").and_then(Value::as_str).unwrap_or("");
        let waiting = reg.get("waitingFor").and_then(Value::as_str).map_or(false, |s| !s.is_empty()) || status.contains("wait");
        let kind = if waiting { "waiting" } else if status == "busy" { "busy" } else { "idle" };
        let started = reg.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
        let updated = reg.get("statusUpdatedAt").and_then(Value::as_u64).unwrap_or(started);
        // a session can have more than one process: the one that changed last is the live one
        if best.get(id).map_or(true, |b| updated > b.updated) {
            best.insert(id.to_string(), Live { id: id.to_string(), kind, updated });
        }
    }
    best.into_values().collect()
}

/// is the window that holds this session the one in front? Then the person can see it already.
fn host_in_front(exe: &Option<String>, icon: &str, project: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};
    let Some(exe) = exe.as_deref() else { return false };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut sys = System::new();
        let p = sysinfo::Pid::from_u32(pid);
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::new().with_exe(UpdateKind::OnlyIfNotSet));
        let name = |s: &str| basename(s).to_lowercase();
        let same_app = sys.process(p).and_then(|p| p.exe()).map_or(false, |e| name(&e.to_string_lossy()) == name(exe));
        if !same_app {
            return false;
        }
        // an editor holds one window per folder: only the one that shows this project counts
        if icon == "term" {
            return true;
        }
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n]).to_lowercase().contains(&project.to_lowercase())
    }
}

fn brief(state: &Arc<crate::IslandState>, id: &str, kind: &'static str) {
    if state.hub_is_open() || state.has_game.load(std::sync::atomic::Ordering::Relaxed) {
        return; // the hub shows the cards already; a game is not interrupted
    }
    let Some(c) = cards().into_iter().find(|c| c.id == id) else { return };
    if host_in_front(&c.host_exe, c.host_icon, &c.project) {
        return;
    }
    let ctx = if c.context_limit > 0 { (c.context_tokens as f64 / c.context_limit as f64).min(1.0) } else { 0.0 };
    state.notif.lock().unwrap().push_brief(
        c.title.clone(),
        crate::notify::Brief { id: c.id.clone(), state: kind, host_icon: c.host_icon, host_exe: c.host_exe.clone(), project: c.project.clone(), ctx },
    );
}

/// Watches the registry files (a few tiny reads every 2 s) and queues a brief-show when a
/// session starts waiting for you or finishes a turn that took a while.
pub fn spawn(_app: AppHandle, state: Arc<crate::IslandState>) {
    std::thread::spawn(move || {
        // id -> (kind, when the current busy stretch began)
        let mut seen: HashMap<String, (&'static str, u64)> = HashMap::new();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let on = {
                let s = state.settings.lock().unwrap();
                s.llm_detection && s.llm_brief
            };
            if !on {
                seen.clear();
                continue;
            }
            let mut present = HashSet::new();
            for s in live_sessions() {
                present.insert(s.id.clone());
                let prev = seen.get(&s.id).copied();
                match (prev.map(|p| p.0), s.kind) {
                    (Some("waiting"), k) if k != "waiting" => state.notif.lock().unwrap().cancel_brief(&s.id),
                    (p, "waiting") if p != Some("waiting") => brief(&state, &s.id, "waiting"),
                    // a turn that took no more than a few seconds is a quick answer, not news
                    (Some("busy"), "idle") if s.updated.saturating_sub(prev.map_or(0, |p| p.1)) >= 5000 => brief(&state, &s.id, "finished"),
                    _ => {}
                }
                let busy_since = match prev {
                    Some(("busy", t)) if s.kind == "busy" => t,
                    _ => s.updated,
                };
                seen.insert(s.id, (s.kind, busy_since));
            }
            seen.retain(|id, _| present.contains(id));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_transcript_tail() {
        let t = concat!(
            r#"{"type":"ai-title","aiTitle":"Fix the thing","sessionId":"s"}"#, "\n",
            r#"{"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"please   fix it"}}"#, "\n",
            r#"{"type":"last-prompt","lastPrompt":"a queued one","sessionId":"s"}"#, "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<system-reminder>x</system-reminder>"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":90000},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"C:\\a\\b\\lib.rs"}}]}}"#, "\n",
            r#"{"type":"user","toolUseResult":{},"message":{"content":[]}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo build --release"}}]}}"#, "\n",
            "not json at all\n",
            r#"{"type":"cost-state","totalCostUSD":1.5,"totalLinesAdded":12,"totalLinesRemoved":3}"#, "\n",
        );
        let p = parse_transcript(t);
        assert_eq!(p.title.as_deref(), Some("Fix the thing"));
        assert_eq!(p.prompt.as_deref(), Some("please fix it"));
        assert_eq!(p.context_tokens, 90010);
        assert_eq!(p.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(p.last.as_ref().unwrap().text, "Running cargo build --release");
        assert_eq!(p.feed.len(), 2);
        assert_eq!(p.feed[1].text, "Editing lib.rs");
        assert_eq!((p.lines_added, p.lines_removed), (12, 3));
        assert!((p.cost_usd - 1.5).abs() < 1e-9);
    }

    #[test]
    fn names_models() {
        assert_eq!(model_name("claude-sonnet-5-5"), "Sonnet 5.5");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(model_name(""), "");
    }

    #[test]
    fn reads_the_list_of_running_distros() {
        // wsl.exe writes UTF-16 with a byte order mark and CRLF line ends
        let text = "\u{feff}Ubuntu-22.04\r\ndocker-desktop\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(running_distros(&decode_wsl_output(&bytes)), ["Ubuntu-22.04"]);
        // the plain-text message of an older wsl.exe is not a distro
        assert!(running_distros("There are no running distributions.\r\n").is_empty());
    }

    #[test]
    fn a_wsl_session_without_a_readable_proc_counts_while_it_is_recent() {
        let root = Root { dir: PathBuf::from(r"Z:\none\.claude"), wsl: Some(("x".into(), PathBuf::from(r"Z:\none"))) };
        let fresh = serde_json::json!({ "statusUpdatedAt": now_ms() });
        let old = serde_json::json!({ "statusUpdatedAt": 1u64 });
        assert!(session_alive(&root, 4242, &fresh));
        assert!(!session_alive(&root, 4242, &old));
    }

    #[test]
    fn lists_the_sessions_of_this_machine() {
        // must never panic, whatever is (or is not) installed
        let _ = cards();
    }
}
