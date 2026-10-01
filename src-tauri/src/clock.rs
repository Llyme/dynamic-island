//! Time announcements: every so often the island drops down as a session pill that says what time
//! it is. Aligned to the clock (every 30 minutes means :00 and :30), so it never needs to count.

use crate::IslandState;
use chrono::{Datelike, Local, Timelike};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::{Manager, WebviewWindow};

/// the steps the setting offers, in minutes
pub const INTERVALS: [u64; 8] = [5, 10, 15, 30, 60, 120, 180, 360];

/// "3:30 PM" or "15:30", and a short date for the right of the pill
fn texts(now: &chrono::DateTime<Local>, h24: bool) -> (String, String) {
    let time = if h24 {
        now.format("%H:%M").to_string()
    } else {
        now.format("%I:%M %p").to_string().trim_start_matches('0').to_string()
    };
    (time, format!("{}, {} {}", now.format("%a"), now.format("%b"), now.day()))
}

pub(crate) fn push(state: &Arc<IslandState>, h24: bool) {
    let (time, date) = texts(&Local::now(), h24);
    state.notif.lock().unwrap().push_brief(
        time,
        crate::notify::Brief { id: "time".into(), state: "time", host_icon: "clock", host_exe: None, project: date, ctx: 0.0 },
    );
}

pub fn spawn(state: Arc<IslandState>) {
    std::thread::spawn(move || {
        // (day, minute of the day) of the last announcement: one per minute mark
        let mut last: Option<(u32, u32)> = None;
        loop {
            std::thread::sleep(Duration::from_secs(10));
            let (on, every, h24) = {
                let s = state.settings.lock().unwrap();
                (s.time_announce, INTERVALS[(s.time_interval as usize).min(INTERVALS.len() - 1)], s.time_24h)
            };
            if !on {
                continue;
            }
            let now = Local::now();
            let minute = now.hour() * 60 + now.minute();
            let mark = (now.ordinal(), minute);
            if u64::from(minute) % every != 0 || last == Some(mark) {
                continue;
            }
            last = Some(mark);
            // not over the hub, not in a game
            if state.hub_is_open() || state.has_game.load(Ordering::Relaxed) {
                continue;
            }
            push(&state, h24);
        }
    });
}

/// the Settings > Time "Show now" button
#[tauri::command]
pub fn time_preview(window: WebviewWindow) {
    let state = window.state::<Arc<IslandState>>().inner().clone();
    let h24 = state.settings.lock().unwrap().time_24h;
    push(&state, h24);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn writes_the_time_both_ways() {
        let t = Local.with_ymd_and_hms(2026, 10, 1, 15, 5, 0).unwrap();
        assert_eq!(texts(&t, true).0, "15:05");
        assert_eq!(texts(&t, false).0, "3:05 PM");
        assert_eq!(texts(&t, false).1, "Thu, Oct 1");
        let am = Local.with_ymd_and_hms(2026, 10, 1, 0, 30, 0).unwrap();
        assert_eq!(texts(&am, false).0, "12:30 AM");
    }
}
