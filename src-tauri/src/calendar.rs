//! Calendar reminders + the hub's calendar view from an `.ics` feed -- port of
//! `parse_ics`/`fetch_ics_events`/`_check_calendar_reminders` from `main.py`.
//! Minimal ICS: VEVENT DTSTART/DTEND/SUMMARY/UID plus a small RRULE subset
//! (DAILY/WEEKLY+BYDAY/MONTHLY/YEARLY, INTERVAL, COUNT, UNTIL, EXDATE and
//! RECURRENCE-ID overrides), expanded over a window around today.

use crate::IslandState;
use chrono::{Datelike, Duration as CDuration, Local, Months, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

const CHECK_S: u64 = 15;
/// how far around today the calendar view can browse (events outside are dropped)
const WINDOW_PAST_DAYS: i64 = 92;
const WINDOW_FUTURE_DAYS: i64 = 400;
const MAX_EVENTS: usize = 4000;
const MAX_OCCURRENCES_PER_RULE: usize = 3000;

#[derive(Clone, Serialize)]
pub struct CalendarEvent {
    pub uid: String,
    pub summary: String,
    pub start_ms: i64,
    /// exclusive end (== start for zero-length events)
    pub end_ms: i64,
    pub all_day: bool,
}

#[derive(Clone, Serialize, Default)]
pub struct CalendarSnapshot {
    pub configured: bool,
    pub error: Option<String>,
    pub events: Vec<CalendarEvent>,
}

#[derive(Default)]
pub struct CalendarState {
    last: Mutex<CalendarSnapshot>,
    notified: Mutex<HashSet<String>>,
    /// set by save_settings when the url changes, so the poll thread refetches now
    pub refetch: AtomicBool,
}

fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push_str(&line[1..]);
            }
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// Returns (start ms since epoch, all_day).
fn parse_datetime(value: &str, is_date_param: bool) -> Option<(i64, bool)> {
    let value = value.trim();
    if is_date_param || (value.len() == 8 && !value.contains('T')) {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let dt = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()?;
        return Some((dt.timestamp_millis(), true));
    }
    if let Some(stripped) = value.strip_suffix('Z') {
        let n = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some((Utc.from_utc_datetime(&n).timestamp_millis(), false));
    }
    let n = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    let dt = Local.from_local_datetime(&n).earliest()?;
    Some((dt.timestamp_millis(), false))
}

#[derive(Default)]
struct RawEvent {
    start: Option<(i64, bool)>,
    end: Option<i64>,
    summary: Option<String>,
    uid: Option<String>,
    rrule: Option<String>,
    exdates: Vec<i64>,
    recurrence_id: Option<i64>,
}

fn local_naive(ms: i64) -> Option<NaiveDateTime> {
    Local.timestamp_millis_opt(ms).earliest().map(|d| d.naive_local())
}

fn from_naive(n: NaiveDateTime) -> Option<i64> {
    Local.from_local_datetime(&n).earliest().map(|d| d.timestamp_millis())
}

fn weekday_from_code(code: &str) -> Option<Weekday> {
    // strip a leading ordinal like "2MO" / "-1FR"
    let letters: String = code.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    match letters.to_uppercase().as_str() {
        "MO" => Some(Weekday::Mon),
        "TU" => Some(Weekday::Tue),
        "WE" => Some(Weekday::Wed),
        "TH" => Some(Weekday::Thu),
        "FR" => Some(Weekday::Fri),
        "SA" => Some(Weekday::Sat),
        "SU" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Expands one RRULE into occurrence start times (ms), from the event's own
/// start up to `window_end`. Stepping is done in local wall-clock time.
fn expand_rrule(start_ms: i64, rule: &str, window_end: i64) -> Vec<i64> {
    let mut freq = "";
    let mut interval: u32 = 1;
    let mut count: Option<usize> = None;
    let mut until: Option<i64> = None;
    let mut byday: Vec<Weekday> = Vec::new();
    for part in rule.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k.to_uppercase().as_str() {
            "FREQ" => freq = v,
            "INTERVAL" => interval = v.parse().unwrap_or(1).max(1),
            "COUNT" => count = v.parse().ok(),
            "UNTIL" => {
                until = parse_datetime(v, v.len() == 8)
                    .map(|(ms, all_day)| if all_day { ms + 86_400_000 - 1 } else { ms })
            }
            "BYDAY" => byday = v.split(',').filter_map(weekday_from_code).collect(),
            _ => {}
        }
    }
    let Some(start) = local_naive(start_ms) else { return vec![start_ms] };
    let limit = until.map_or(window_end, |u| u.min(window_end));
    let time = start.time();
    let max_n = count.unwrap_or(usize::MAX).min(MAX_OCCURRENCES_PER_RULE);
    let mut out: Vec<i64> = Vec::new();
    // returns true when generation must stop
    let push = |ms: i64, out: &mut Vec<i64>| -> bool {
        if ms > limit {
            return true;
        }
        out.push(ms);
        out.len() >= max_n
    };
    match freq.to_uppercase().as_str() {
        "DAILY" => {
            for k in 0..MAX_OCCURRENCES_PER_RULE as i64 * 4 {
                let n = start.date() + CDuration::days(k * interval as i64);
                let Some(ms) = from_naive(n.and_time(time)) else { continue };
                if push(ms, &mut out) {
                    break;
                }
            }
        }
        "WEEKLY" => {
            if byday.is_empty() {
                byday.push(start.weekday());
            }
            byday.sort_by_key(|d| d.num_days_from_monday());
            let base = start.date() - CDuration::days(start.weekday().num_days_from_monday() as i64);
            'weeks: for k in 0..MAX_OCCURRENCES_PER_RULE as i64 {
                for wd in &byday {
                    let d = base + CDuration::days(7 * interval as i64 * k + wd.num_days_from_monday() as i64);
                    let n = d.and_time(time);
                    if n < start {
                        continue;
                    }
                    let Some(ms) = from_naive(n) else { continue };
                    if push(ms, &mut out) {
                        break 'weeks;
                    }
                }
            }
        }
        "MONTHLY" | "YEARLY" => {
            let step = if freq.eq_ignore_ascii_case("YEARLY") { 12 } else { 1 } * interval;
            for k in 0..MAX_OCCURRENCES_PER_RULE as u32 {
                // checked_add_months clamps the 31st to month end; RFC 5545 skips those
                let Some(d) = start.date().checked_add_months(Months::new(k * step)) else { break };
                if d.day() != start.day() {
                    continue;
                }
                let Some(ms) = from_naive(d.and_time(time)) else { continue };
                if push(ms, &mut out) {
                    break;
                }
            }
        }
        _ => out.push(start_ms),
    }
    out
}

fn parse_ics(text: &str) -> Vec<CalendarEvent> {
    parse_ics_window(text, Utc::now().timestamp_millis())
}

/// Parses the feed and expands recurrences into a flat, sorted list of events
/// within [now - 92 days, now + 400 days].
fn parse_ics_window(text: &str, now_ms: i64) -> Vec<CalendarEvent> {
    let win_start = now_ms - WINDOW_PAST_DAYS * 86_400_000;
    let win_end = now_ms + WINDOW_FUTURE_DAYS * 86_400_000;

    let mut raws: Vec<RawEvent> = Vec::new();
    let mut cur: Option<RawEvent> = None;
    for line in unfold(text) {
        let stripped = line.trim();
        if stripped == "BEGIN:VEVENT" {
            cur = Some(RawEvent::default());
        } else if stripped == "END:VEVENT" {
            if let Some(ev) = cur.take() {
                raws.push(ev);
            }
        } else if let (Some(c), Some((key_part, value))) = (cur.as_mut(), line.split_once(':')) {
            let mut bits = key_part.split(';');
            let key = bits.next().unwrap_or("").to_uppercase();
            let is_date_param = bits.any(|b| b.eq_ignore_ascii_case("VALUE=DATE"));
            match key.as_str() {
                "DTSTART" => c.start = parse_datetime(value, is_date_param),
                "DTEND" => c.end = parse_datetime(value, is_date_param).map(|(ms, _)| ms),
                "SUMMARY" => {
                    c.summary = Some(value.replace("\\n", " ").replace("\\,", ",").trim().to_string())
                }
                "UID" => c.uid = Some(value.trim().to_string()),
                "RRULE" => c.rrule = Some(value.trim().to_string()),
                "EXDATE" => {
                    for v in value.split(',') {
                        if let Some((ms, _)) = parse_datetime(v, is_date_param) {
                            c.exdates.push(ms);
                        }
                    }
                }
                "RECURRENCE-ID" => c.recurrence_id = parse_datetime(value, is_date_param).map(|(ms, _)| ms),
                _ => {}
            }
        }
    }

    // a moved/edited single instance carries RECURRENCE-ID: hide the master's
    // original occurrence at that time
    let mut overridden: HashMap<String, Vec<i64>> = HashMap::new();
    for r in &raws {
        if let (Some(id), Some(uid)) = (r.recurrence_id, r.uid.as_ref()) {
            overridden.entry(uid.clone()).or_default().push(id);
        }
    }

    let mut events = Vec::new();
    for r in raws {
        let (Some((start_ms, all_day)), Some(summary)) = (r.start, r.summary.clone()) else { continue };
        let duration = match r.end {
            Some(end) if end > start_ms => end - start_ms,
            _ if all_day => 86_400_000,
            _ => 0,
        };
        let uid = r.uid.clone().unwrap_or_else(|| format!("{summary}|{start_ms}"));
        let recurring = r.rrule.is_some() && r.recurrence_id.is_none();
        let starts: Vec<i64> = if recurring {
            let skip: HashSet<i64> = r
                .exdates
                .iter()
                .copied()
                .chain(overridden.get(&uid).into_iter().flatten().copied())
                .collect();
            expand_rrule(start_ms, r.rrule.as_deref().unwrap_or(""), win_end)
                .into_iter()
                .filter(|s| !skip.contains(s))
                .collect()
        } else {
            vec![start_ms]
        };
        for s in starts {
            if s + duration < win_start || s > win_end {
                continue;
            }
            events.push(CalendarEvent {
                // each occurrence needs its own id (reminders are de-duplicated by it)
                uid: if recurring { format!("{uid}|{s}") } else { uid.clone() },
                summary: summary.clone(),
                start_ms: s,
                end_ms: s + duration,
                all_day,
            });
        }
    }
    events.sort_by_key(|e| e.start_ms);
    events.truncate(MAX_EVENTS);
    events
}

fn fetch_events(url: &str) -> Result<Vec<CalendarEvent>, String> {
    let resp = ureq::get(url)
        .set("User-Agent", "NADI/1.0")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                format!("HTTP {code} -- check the .ics link is correct and still valid")
            }
            other => format!("Network error: {other}"),
        })?;
    let text = resp.into_string().map_err(|e| e.to_string())?;
    Ok(parse_ics(&text))
}

fn check_reminders(app: &AppHandle, state: &Arc<IslandState>) {
    let lead_ms = state.settings.lock().unwrap().calendar_reminder_lead_min as i64 * 60_000;
    let now = Utc::now().timestamp_millis();
    let events = state.calendar.last.lock().unwrap().events.clone();
    for ev in events {
        let delta = ev.start_ms - now;
        if delta > 0 && delta <= lead_ms && state.calendar.notified.lock().unwrap().insert(ev.uid.clone()) {
            let mins = (delta / 60_000).max(1);
            let history = {
                let mut notif = state.notif.lock().unwrap();
                notif.push("Upcoming event".into(), format!("{} in {mins} min", ev.summary));
                notif.history()
            };
            let _ = app.emit("notification-history-tick", history);
        }
    }
}

fn emit(app: &AppHandle, state: &Arc<IslandState>, snapshot: CalendarSnapshot) {
    *state.calendar.last.lock().unwrap() = snapshot.clone();
    let _ = app.emit("calendar-tick", snapshot);
}

pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || {
        let mut last_fetch: Option<Instant> = None;
        loop {
            let (url, poll_min) = {
                let s = state.settings.lock().unwrap();
                (s.calendar_ics_url.trim().to_string(), s.calendar_poll_min.max(1))
            };
            if url.is_empty() {
                if state.calendar.last.lock().unwrap().configured {
                    emit(&app, &state, CalendarSnapshot::default());
                }
                last_fetch = None;
            } else {
                let forced = state.calendar.refetch.swap(false, Ordering::Relaxed);
                let due = last_fetch.map_or(true, |t| t.elapsed() >= Duration::from_secs(poll_min * 60));
                if forced || due {
                    last_fetch = Some(Instant::now());
                    let snapshot = match fetch_events(&url) {
                        Ok(events) => CalendarSnapshot { configured: true, error: None, events },
                        Err(e) => CalendarSnapshot { configured: true, error: Some(e), events: Vec::new() },
                    };
                    emit(&app, &state, snapshot);
                }
                check_reminders(&app, &state);
            }
            std::thread::sleep(Duration::from_secs(CHECK_S));
        }
    });
}

#[tauri::command]
pub fn get_calendar(window: WebviewWindow) -> CalendarSnapshot {
    let state = window.state::<Arc<IslandState>>();
    let snapshot = state.calendar.last.lock().unwrap().clone();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_folded_events_and_date_only() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a1\r\nDTSTART:20260930T090000Z\r\nSUMMARY:Standup\\, team\r\n  sync\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261001\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:No start\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ev = parse_ics(ics);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].uid, "a1");
        assert_eq!(ev[0].summary, "Standup, team sync");
        assert!(!ev[0].all_day);
        assert!(ev[1].all_day);
    }

    fn ms(y: i32, m: u32, d: u32, h: u32) -> i64 {
        from_naive(NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()).unwrap()
    }

    #[test]
    fn expands_weekly_rule_with_exdate_and_override() {
        // Mon+Wed standup starting Mon 2026-09-07, 4 occurrences, one excluded, one moved
        let ics = "BEGIN:VEVENT
UID:s
DTSTART:20260907T090000
DTEND:20260907T093000
SUMMARY:Standup
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=4
EXDATE:20260909T090000
END:VEVENT
BEGIN:VEVENT
UID:s
RECURRENCE-ID:20260914T090000
DTSTART:20260914T140000
SUMMARY:Standup (moved)
END:VEVENT
";
        let ev = parse_ics_window(ics, ms(2026, 9, 10, 12));
        let titles: Vec<_> = ev.iter().map(|e| (e.summary.as_str(), e.start_ms)).collect();
        // Mon 7 kept, Wed 9 exdate, Mon 14 moved away (+ its moved instance), Wed 16 kept
        assert_eq!(ev.len(), 3, "{titles:?}");
        assert_eq!(ev[0].start_ms, ms(2026, 9, 7, 9));
        assert_eq!(ev[0].end_ms - ev[0].start_ms, 30 * 60_000);
        assert_eq!(ev[1].start_ms, ms(2026, 9, 14, 14));
        assert_eq!(ev[2].start_ms, ms(2026, 9, 16, 9));
    }

    #[test]
    fn expands_daily_until_and_monthly_skips_missing_days() {
        let ics = "BEGIN:VEVENT
UID:d
DTSTART;VALUE=DATE:20260901
SUMMARY:Daily
RRULE:FREQ=DAILY;UNTIL=20260905
END:VEVENT
BEGIN:VEVENT
UID:m
DTSTART:20260131T100000
SUMMARY:Month end
RRULE:FREQ=MONTHLY;COUNT=3
END:VEVENT
";
        let ev = parse_ics_window(ics, ms(2026, 9, 3, 12));
        assert_eq!(ev.iter().filter(|e| e.summary == "Daily").count(), 5);
        assert!(ev.iter().filter(|e| e.summary == "Month end").count() <= 3);
    }
}
