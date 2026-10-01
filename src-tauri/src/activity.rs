//! What the hub's "Now" section shows: the current game (with playtime), a card
//! for every work category focused within the last couple of minutes, and the
//! coding-stats card (today's total, projects, languages). Port of the
//! `_work_recent`/`_work_stats` machinery from `main.py`; day totals persist to
//! `%APPDATA%\NADI\work_stats.json` (last 30 days).

use crate::browse::BrowseInfo;
use crate::downloads::{DownloadCard, DownloadItem};
use crate::project::ProjectInfo;
use crate::{exeinfo, IslandState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{Manager, WebviewWindow};

/// a category still counts as "going on" this long after it was last focused --
/// lets coding and browsing both show a card when you're bouncing between them
const RECENT_ACTIVE_S: u64 = 30;
const KEEP_DAYS: usize = 30;

const LANGUAGE_EXT: &[(&str, &str)] = &[
    ("py", "Python"), ("pyw", "Python"),
    ("js", "JavaScript"), ("jsx", "JavaScript"), ("mjs", "JavaScript"), ("cjs", "JavaScript"),
    ("ts", "TypeScript"), ("tsx", "TypeScript"),
    ("java", "Java"), ("kt", "Kotlin"), ("kts", "Kotlin"),
    ("c", "C"), ("h", "C"),
    ("cpp", "C++"), ("cc", "C++"), ("cxx", "C++"), ("hpp", "C++"), ("hh", "C++"),
    ("cs", "C#"), ("go", "Go"), ("rs", "Rust"), ("rb", "Ruby"), ("php", "PHP"),
    ("swift", "Swift"), ("m", "Objective-C"), ("mm", "Objective-C"),
    ("scala", "Scala"), ("dart", "Dart"), ("lua", "Lua"), ("r", "R"), ("jl", "Julia"),
    ("hs", "Haskell"), ("ex", "Elixir"), ("exs", "Elixir"), ("erl", "Erlang"),
    ("clj", "Clojure"), ("sql", "SQL"), ("html", "HTML"), ("htm", "HTML"),
    ("css", "CSS"), ("scss", "SCSS"), ("sass", "Sass"), ("less", "Less"),
    ("json", "JSON"), ("yaml", "YAML"), ("yml", "YAML"), ("toml", "TOML"),
    ("xml", "XML"), ("md", "Markdown"), ("sh", "Shell"), ("bash", "Shell"),
    ("zsh", "Shell"), ("ps1", "PowerShell"), ("bat", "Batch"), ("vue", "Vue"),
    ("svelte", "Svelte"), ("proto", "Protobuf"), ("graphql", "GraphQL"),
    ("gql", "GraphQL"), ("ipynb", "Jupyter Notebook"),
];

const LANGUAGE_COLORS: &[(&str, &str)] = &[
    ("Python", "#4B8BBE"), ("TypeScript", "#3178C6"), ("JavaScript", "#E8C547"),
    ("CSS", "#4F9DE0"), ("SCSS", "#4F9DE0"), ("Sass", "#CF649A"), ("Less", "#1D365D"),
    ("JSON", "#8BC34A"), ("YAML", "#CB171E"), ("TOML", "#9C4221"), ("XML", "#F1662A"),
    ("Markdown", "#9AA0A6"), ("HTML", "#E44D26"), ("Shell", "#89E051"),
    ("PowerShell", "#4273CA"), ("Batch", "#C1F12E"), ("Rust", "#DEA584"),
    ("Go", "#00ADD8"), ("Java", "#B07219"), ("Kotlin", "#A97BFF"), ("C++", "#F34B7D"),
    ("C", "#8595AB"), ("C#", "#178600"), ("Ruby", "#CC342D"), ("PHP", "#8892BF"),
    ("Swift", "#F05138"), ("Dart", "#00B4AB"), ("Scala", "#C22D40"), ("Lua", "#000080"),
    ("R", "#198CE7"), ("Vue", "#41B883"), ("Svelte", "#FF3E00"),
];

/// window-title segments that are the editor's own name, not a project
const EDITOR_APP_NAMES: &[&str] = &[
    "visual studio code", "visual studio code - insiders", "code", "vs code",
    "cursor", "sublime text", "notepad++", "android studio", "pycharm",
    "intellij idea", "webstorm", "clion", "rider", "goland", "phpstorm",
    "vim", "neovim",
];

/// (project, language) guessed from an editor's window title, best-effort.
/// Most editors title their window "file.ext - project - App Name", with an
/// optional leading bullet on an unsaved file.
pub fn parse_coding_context(title: &str) -> (Option<String>, Option<String>) {
    if title.is_empty() {
        return (None, None);
    }
    let normalized = title.replace(" — ", " - ").replace(" – ", " - ");
    let parts: Vec<String> = normalized
        .split(" - ")
        .map(|p| p.trim_matches(|c: char| c == ' ' || c == '●' || c == '•' || c == '∙').to_string())
        .filter(|p| !p.is_empty() && !EDITOR_APP_NAMES.contains(&p.to_lowercase().as_str()))
        .collect();
    let Some(filename) = parts.first() else {
        return (None, None);
    };
    let project = if parts.len() >= 2 { parts.last().cloned() } else { None };
    let language = filename.rsplit_once('.').and_then(|(_, ext)| {
        let ext = ext.to_lowercase();
        LANGUAGE_EXT.iter().find(|(e, _)| *e == ext).map(|(_, l)| l.to_string())
    });
    (project, language)
}

/// (file, unsaved) from an editor window title: "● main.rs - project - Visual Studio Code"
pub fn parse_editor_file(title: &str) -> (Option<String>, bool) {
    let normalized = title.replace(" — ", " - ").replace(" – ", " - ");
    let first = normalized.split(" - ").next().unwrap_or("").trim();
    let unsaved = first.starts_with('●') || first.starts_with('•') || first.starts_with('∙');
    let file = first.trim_matches(|c: char| c == ' ' || c == '●' || c == '•' || c == '∙').to_string();
    let is_editor = EDITOR_APP_NAMES.contains(&file.to_lowercase().as_str());
    // the first title segment is not always a file: chat/settings/preview tabs
    // title themselves ("Settings", "Brief-show island queue ..."). Accept only
    // something that looks like a file name.
    let looks_like_file = file.chars().count() <= 80
        && !file.ends_with('…')
        && (file.rsplit_once('.').map_or(false, |(stem, ext)| {
            !stem.is_empty() && (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }) || ["makefile", "dockerfile", "license", "readme", "procfile"].contains(&file.to_lowercase().as_str()));
    ((!file.is_empty() && !is_editor && looks_like_file).then_some(file), unsaved)
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct DayStats {
    #[serde(default)]
    categories: HashMap<String, f64>,
    #[serde(default)]
    projects: HashMap<String, f64>,
    #[serde(default)]
    languages: HashMap<String, f64>,
}

#[derive(Serialize, Deserialize, Default)]
struct WorkStats {
    #[serde(default)]
    days: BTreeMap<String, DayStats>,
}

struct RecentCat {
    last_seen: Instant,
    started_at: Instant,
    app_name: String,
    exe_path: Option<String>,
}

struct GameInfo {
    name: String,
    exe_path: String,
    since: Instant,
    pid: u32,
}

#[derive(Default)]
struct Inner {
    /// running games, most recently focused first
    games: Vec<GameInfo>,
    recent: HashMap<String, RecentCat>,
    stats: WorkStats,
    dirty: bool,
    active_project: String,
    /// the confirmed work session (category, started) -- same one the pill shows
    session: Option<(String, Instant)>,
    /// what the editor showed last time coding was observed
    coding_file: Option<String>,
    coding_unsaved: bool,
    /// the browser in front last time browsing was observed, and its page title
    browse_exe: Option<String>,
    browse_title: String,
}

pub struct ActivityState {
    inner: Mutex<Inner>,
    /// git/branch/changed-files of the open project, refreshed by `project::spawn`
    pub project: Mutex<Option<ProjectInfo>>,
    /// today's browsing from the browser history, refreshed by `browse::spawn`
    pub browsing: Mutex<Option<BrowseInfo>>,
    /// what is downloading right now (and finished ones not yet dismissed), kept by `downloads::spawn`
    pub downloads: Mutex<Vec<DownloadItem>>,
}

impl Default for ActivityState {
    fn default() -> Self {
        let stats = stats_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self { inner: Mutex::new(Inner { stats, ..Default::default() }), project: Mutex::new(None), browsing: Mutex::new(None), downloads: Mutex::new(Vec::new()) }
    }
}

fn stats_path() -> Option<PathBuf> {
    Some(crate::settings::data_dir()?.join("work_stats.json"))
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl ActivityState {
    pub fn active_project_name(&self) -> String {
        self.inner.lock().unwrap().active_project.clone()
    }

    /// (browser exe, page title) while a browsing card is showing
    pub fn active_browser(&self) -> Option<(String, String)> {
        let g = self.inner.lock().unwrap();
        let fresh = g.recent.get("browsing").map_or(false, |r| r.last_seen.elapsed() <= Duration::from_secs(RECENT_ACTIVE_S));
        fresh.then(|| g.browse_exe.clone().map(|e| (e, g.browse_title.clone()))).flatten()
    }

    pub fn set_games(&self, games: Vec<(String, String, Instant, u32)>) {
        self.inner.lock().unwrap().games = games
            .into_iter()
            .map(|(name, exe_path, since, pid)| GameInfo { name, exe_path, since, pid })
            .collect();
    }

    pub fn set_session(&self, session: Option<(String, Instant)>) {
        self.inner.lock().unwrap().session = session;
    }

    /// One work-poll observation: `elapsed` seconds spent in `category`.
    pub fn observe(
        &self,
        category: &str,
        elapsed: f64,
        title: &str,
        app_name: &str,
        exe_path: Option<&str>,
    ) {
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        if category == "coding" {
            let (project, _) = parse_coding_context(title);
            let (file, unsaved) = parse_editor_file(title);
            // a tab that is not a file (chat, settings) keeps showing the last real file
            if file.is_some() {
                g.coding_file = file;
                g.coding_unsaved = unsaved;
            }
            g.active_project = project
                .or_else(|| (!app_name.is_empty()).then(|| app_name.to_string()))
                .unwrap_or_else(|| "your editor".into());
        }
        if category == "browsing" {
            g.browse_exe = exe_path.map(str::to_string);
            g.browse_title = crate::browse::clean_title(title);
        }
        if elapsed > 0.0 {
            let bucket = g.stats.days.entry(today()).or_default();
            *bucket.categories.entry(category.to_string()).or_insert(0.0) += elapsed;
            if category == "coding" {
                let (project, language) = parse_coding_context(title);
                if let Some(p) = project {
                    *bucket.projects.entry(p).or_insert(0.0) += elapsed;
                }
                if let Some(l) = language {
                    *bucket.languages.entry(l).or_insert(0.0) += elapsed;
                }
            }
            g.dirty = true;
        }
        // "other" (unrecognized apps) still counts toward stats but never gets a card
        if category == "other" {
            return;
        }
        let prev_started = g
            .recent
            .get(category)
            .filter(|r| r.last_seen.elapsed() <= Duration::from_secs(RECENT_ACTIVE_S))
            .map(|r| r.started_at);
        let prev_name = g.recent.get(category).map(|r| r.app_name.clone()).unwrap_or_default();
        g.recent.insert(
            category.to_string(),
            RecentCat {
                last_seen: now,
                started_at: prev_started.unwrap_or(now),
                app_name: if app_name.is_empty() { prev_name } else { app_name.to_string() },
                exe_path: exe_path.map(str::to_string),
            },
        );
    }

    pub fn flush(&self) {
        let mut g = self.inner.lock().unwrap();
        if !g.dirty {
            return;
        }
        while g.stats.days.len() > KEEP_DAYS {
            let Some(oldest) = g.stats.days.keys().next().cloned() else { break };
            g.stats.days.remove(&oldest);
        }
        g.dirty = false;
        if let (Some(path), Ok(json)) = (stats_path(), serde_json::to_string_pretty(&g.stats)) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, json);
        }
    }
}

#[derive(Serialize)]
pub struct GameCard {
    pid: u32,
    name: String,
    exe_path: String,
    icon: Option<String>,
    playtime_secs: f64,
}

#[derive(Serialize)]
pub struct CodingCard {
    exe_path: Option<String>,
    today_secs: f64,
    active: bool,
    project: String,
    active_secs: f64,
    file: Option<String>,
    language: Option<String>,
    language_color: Option<&'static str>,
    unsaved: bool,
    project_info: Option<ProjectInfo>,
}

#[derive(Serialize)]
pub struct WorkCard {
    exe_path: Option<String>,
    category: String,
    app_name: String,
    icon: Option<String>,
    going_secs: f64,
    /// browsing only: page in front + today's history
    page: Option<String>,
    browse: Option<BrowseInfo>,
}

#[derive(Serialize, Default)]
pub struct ActivitySnapshot {
    games: Vec<GameCard>,
    downloads: Vec<DownloadCard>,
    /// running AI sessions (Claude Code), the ones waiting for you first
    llm: Vec<crate::llm::LlmCard>,
    /// every non-coding category focused recently, newest streak first
    work: Vec<WorkCard>,
    coding: Option<CodingCard>,
}

#[tauri::command]
pub fn get_activity(window: WebviewWindow) -> ActivitySnapshot {
    let state = window.state::<Arc<IslandState>>();
    let work_on = state.work_detection_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let g = state.activity.inner.lock().unwrap();
    let mut snap = ActivitySnapshot::default();

    for game in &g.games {
        snap.games.push(GameCard {
            pid: game.pid,
            name: game.name.clone(),
            exe_path: game.exe_path.clone(),
            icon: exeinfo::lookup(&game.exe_path).icon,
            playtime_secs: game.since.elapsed().as_secs_f64(),
        });
    }
    if state.settings.lock().unwrap().llm_detection {
        snap.llm = crate::llm::cards();
    }
    if state.settings.lock().unwrap().download_detection {
        snap.downloads = crate::downloads::cards(&state.activity.downloads.lock().unwrap());
    }
    if !work_on {
        return snap;
    }

    let mut recent: Vec<_> = g
        .recent
        .iter()
        .filter(|(_, r)| r.last_seen.elapsed() <= Duration::from_secs(RECENT_ACTIVE_S))
        .collect();
    recent.sort_by_key(|(_, r)| std::cmp::Reverse(r.started_at));
    for (category, r) in recent {
        if category == "coding" {
            let day = g.stats.days.get(&today()).cloned().unwrap_or_default();
            let session = g.session.as_ref().filter(|(c, _)| c == "coding");
            let language = g.coding_file.as_deref().and_then(|f| {
                f.rsplit_once('.').and_then(|(_, ext)| {
                    let ext = ext.to_lowercase();
                    LANGUAGE_EXT.iter().find(|(e, _)| *e == ext).map(|(_, l)| l.to_string())
                })
            });
            snap.coding = Some(CodingCard {
                exe_path: r.exe_path.clone(),
                today_secs: day.categories.get("coding").copied().unwrap_or(0.0),
                active: session.is_some(),
                project: g.active_project.clone(),
                active_secs: session.map_or(0.0, |(_, t)| t.elapsed().as_secs_f64()),
                language: language.clone(),
                language_color: language
                    .as_deref()
                    .and_then(|l| LANGUAGE_COLORS.iter().find(|(n, _)| *n == l).map(|(_, c)| *c)),
                file: g.coding_file.clone(),
                unsaved: g.coding_unsaved,
                project_info: state.activity.project.lock().unwrap().clone(),
            });
        } else {
            snap.work.push(WorkCard {
                exe_path: r.exe_path.clone(),
                category: category.clone(),
                app_name: r.app_name.clone(),
                icon: r.exe_path.as_deref().and_then(|p| exeinfo::lookup(p).icon),
                going_secs: r.started_at.elapsed().as_secs_f64(),
                page: (category == "browsing").then(|| g.browse_title.clone()).filter(|t| !t.is_empty()),
                browse: if category == "browsing" { state.activity.browsing.lock().unwrap().clone() } else { None },
            });
        }
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_file_only_when_it_looks_like_a_file() {
        assert_eq!(
            parse_editor_file("● main.rs - dynamic-island - Visual Studio Code"),
            (Some("main.rs".to_string()), true)
        );
        assert_eq!(parse_editor_file("Cargo.toml - proj - Visual Studio Code").0.as_deref(), Some("Cargo.toml"));
        // chat / settings tabs are not files
        assert_eq!(parse_editor_file("Brief-show island queue … - dynamic-island - Visual Studio Code").0, None);
        assert_eq!(parse_editor_file("Settings - dynamic-island - Visual Studio Code").0, None);
    }

    #[test]
    fn parses_vscode_title() {
        let (p, l) = parse_coding_context("● main.rs - dynamic-island - Visual Studio Code");
        assert_eq!(p.as_deref(), Some("dynamic-island"));
        assert_eq!(l.as_deref(), Some("Rust"));
        let (p, l) = parse_coding_context("Untitled - Notepad++");
        assert_eq!(p, None);
        assert_eq!(l, None);
        assert_eq!(parse_coding_context(""), (None, None));
    }
}
