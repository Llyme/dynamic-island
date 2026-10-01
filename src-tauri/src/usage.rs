//! Claude usage (5-hour / 7-day quota windows) -- port of
//! `fetch_anthropic_oauth_usage` and friends from `main.py`. Reuses Claude
//! Code CLI's own stored login (`~/.claude/.credentials.json`) instead of a
//! separate OAuth flow; a refreshed token pair is cached in memory only and
//! never written anywhere (the CLI's credentials file stays the source of truth).

use crate::IslandState;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA: &str = "oauth-2025-04-20";
// Cloudflare in front of both hosts blocks default HTTP-library UAs; a small
// honestly-named tool UA (same one the Python version settled on) gets through.
const USER_AGENT: &str = "claude-code/2.1.0";
const POLL_S: u64 = 120; // 5s got the endpoint rate-limited -- back off to 2min
const MANUAL_COOLDOWN_S: u64 = 60;

#[derive(Clone)]
struct Tokens {
    access: String,
    refresh: String,
    expires_ms: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageWindow {
    pub pct: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageSnapshot {
    /// false when there's no Claude Code login on this machine -- the
    /// frontend hides the rings entirely rather than showing an error
    pub available: bool,
    pub error: Option<String>,
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}

#[derive(Default)]
pub struct UsageState {
    tokens: Mutex<Option<Tokens>>,
    last_manual: Mutex<Option<Instant>>,
    last: Mutex<UsageSnapshot>,
    /// last seen (5h, 7d) percentages, to detect a change worth peeking about
    last_pct: Mutex<(Option<f64>, Option<f64>)>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn credentials_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

fn has_credentials() -> bool {
    credentials_path().map(|p| p.exists()).unwrap_or(false)
}

fn import_credentials() -> Result<Tokens, String> {
    let path = credentials_path().ok_or("no home directory")?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Couldn't read Claude Code CLI credentials: {e}"))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Couldn't parse Claude Code CLI credentials: {e}"))?;
    let oauth = &v["claudeAiOauth"];
    let access = oauth["accessToken"]
        .as_str()
        .ok_or("Claude Code CLI credentials have no accessToken")?
        .to_string();
    Ok(Tokens {
        access,
        refresh: oauth["refreshToken"].as_str().unwrap_or("").to_string(),
        expires_ms: oauth["expiresAt"].as_u64().unwrap_or(0),
    })
}

fn refresh_token(refresh: &str) -> Result<Tokens, String> {
    let resp = ureq::post(TOKEN_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(20))
        .send_json(json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": CLIENT_ID,
        }))
        .map_err(|e| match e {
            ureq::Error::Status(429, _) => "Rate limited by Anthropic while refreshing the login.".to_string(),
            ureq::Error::Status(code, _) => format!("HTTP {code} refreshing the Claude login."),
            other => format!("Network error: {other}"),
        })?;
    let data: Value = resp.into_json().map_err(|e| e.to_string())?;
    let access = data["access_token"]
        .as_str()
        .ok_or("Token refresh returned no access_token")?
        .to_string();
    let expires_in = data["expires_in"].as_u64().unwrap_or(0);
    Ok(Tokens {
        access,
        refresh: data["refresh_token"].as_str().unwrap_or(refresh).to_string(),
        expires_ms: if expires_in > 0 { now_ms() + expires_in * 1000 } else { 0 },
    })
}

fn call_usage(access: &str) -> Result<(u16, Value), String> {
    let result = ureq::get(USAGE_URL)
        .set("Authorization", &format!("Bearer {access}"))
        .set("anthropic-beta", BETA)
        .set("Accept", "application/json")
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(15))
        .call();
    match result {
        Ok(resp) => {
            let status = resp.status();
            Ok((status, resp.into_json().unwrap_or(Value::Null)))
        }
        Err(ureq::Error::Status(code, resp)) => Ok((code, resp.into_json().unwrap_or(Value::Null))),
        Err(e) => Err(format!("Network error: {e}")),
    }
}

fn parse_window(usage: &Value, key: &str) -> Option<UsageWindow> {
    let w = usage.get(key)?;
    if !w.is_object() {
        return None;
    }
    Some(UsageWindow {
        pct: w["utilization"].as_f64(),
        resets_at: w["resets_at"].as_str().map(str::to_string),
    })
}

fn fetch(state: &UsageState) -> Result<UsageSnapshot, String> {
    let now = now_ms();
    let cached = state.tokens.lock().unwrap().clone();
    let mut tokens = match cached {
        Some(t) if t.expires_ms > now + 60_000 => t,
        _ => {
            let imported = import_credentials()?;
            if imported.expires_ms != 0 && imported.expires_ms < now + 60_000 {
                refresh_token(&imported.refresh)?
            } else {
                imported
            }
        }
    };
    *state.tokens.lock().unwrap() = Some(tokens.clone());

    let (mut status, mut data) = call_usage(&tokens.access)?;
    if status == 401 && !tokens.refresh.is_empty() {
        tokens = refresh_token(&tokens.refresh)?;
        *state.tokens.lock().unwrap() = Some(tokens.clone());
        (status, data) = call_usage(&tokens.access)?;
    }
    match status {
        200 => {}
        401 => return Err("Claude Code CLI session expired -- run \"claude login\" again.".into()),
        429 => return Err("Rate limited by the usage endpoint -- try again in a bit.".into()),
        other => return Err(format!("HTTP {other}")),
    }

    let usage = if data.get("five_hour").is_none() && data.get("seven_day").is_none() {
        data.get("usage").or_else(|| data.get("data")).unwrap_or(&data).clone()
    } else {
        data
    };
    Ok(UsageSnapshot {
        available: true,
        error: None,
        five_hour: parse_window(&usage, "five_hour"),
        seven_day: parse_window(&usage, "seven_day"),
    })
}

/// Brief island peek when either usage percentage moved (rounded, so
/// sub-percent jitter is ignored). Skipped while the hub/a notification owns
/// the island, but the new baseline is still recorded so no delta is lost.
fn maybe_peek(app: &AppHandle, state: &Arc<IslandState>, snapshot: &UsageSnapshot) {
    if snapshot.error.is_some() || !snapshot.available {
        return;
    }
    let new = (
        snapshot.five_hour.as_ref().and_then(|w| w.pct),
        snapshot.seven_day.as_ref().and_then(|w| w.pct),
    );
    let old = std::mem::replace(&mut *state.usage.last_pct.lock().unwrap(), new);
    let moved = |o: Option<f64>, n: Option<f64>| {
        matches!((o, n), (Some(o), Some(n)) if o.round() != n.round())
    };
    if !(moved(old.0, new.0) || moved(old.1, new.1)) {
        return;
    }
    if state.hub_is_open() || state.notif.lock().unwrap().has_current() {
        return;
    }
    let _ = app.emit(
        "usage-peek",
        json!({ "before": { "five_hour": old.0, "seven_day": old.1 },
                "after": { "five_hour": new.0, "seven_day": new.1 } }),
    );
    *state.usage_peek_until.lock().unwrap() =
        Some(Instant::now() + Duration::from_millis(crate::USAGE_PEEK_TOTAL_MS));
}

fn refresh_now(app: &AppHandle, state: &Arc<IslandState>) {
    let snapshot = if !has_credentials() {
        UsageSnapshot::default()
    } else {
        match fetch(&state.usage) {
            Ok(s) => s,
            Err(e) => UsageSnapshot {
                available: true,
                error: Some(e),
                ..Default::default()
            },
        }
    };
    *state.usage.last.lock().unwrap() = snapshot.clone();
    maybe_peek(app, state, &snapshot);
    let _ = app.emit("usage-tick", snapshot);
}

pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || loop {
        refresh_now(&app, &state);
        std::thread::sleep(Duration::from_secs(POLL_S));
    });
}

#[tauri::command]
pub fn get_usage(window: WebviewWindow) -> UsageSnapshot {
    let state = window.state::<Arc<IslandState>>();
    let snapshot = state.usage.last.lock().unwrap().clone();
    snapshot
}

/// Manual refresh from a ring click, with a cooldown so it can't be spammed
/// into the endpoint's rate limit. Returns whether a refresh actually ran.
#[tauri::command]
pub fn refresh_usage(window: WebviewWindow) -> bool {
    let state = window.state::<Arc<IslandState>>().inner().clone();
    {
        let mut last = state.usage.last_manual.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < Duration::from_secs(MANUAL_COOLDOWN_S) {
                return false;
            }
        }
        *last = Some(Instant::now());
    }
    let app = window.app_handle().clone();
    std::thread::spawn(move || refresh_now(&app, &state));
    true
}
