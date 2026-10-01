//! Notification banners -- port of `notify()`/`_dismiss_notification` from
//! `main.py`, including the queueing behavior added on top of it later (a
//! notification arriving while one is already showing queues behind it
//! instead of stomping the current one or getting silently dropped).
//!
//! State lives in a `Mutex` rather than atomics -- title/body strings and a
//! queue don't fit the lock-free primitives the rest of `IslandState` uses,
//! and this only changes a few times a minute, so a mutex is plenty cheap.

use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const NOTIF_MS: u64 = 4000;
const HISTORY_CAP: usize = 50;
/// a chat app that sends more than this many notifications at once (not yet looked at) is
/// shown as one "N new messages" instead
const GROUP_AFTER: u32 = 3;

/// a Claude Code session that needs you, or finished: shown as a compact pill, not a banner
#[derive(Clone, Serialize)]
pub struct Brief {
    /// the session id
    pub id: String,
    /// waiting | finished
    pub state: &'static str,
    pub host_icon: &'static str,
    pub host_exe: Option<String>,
    pub project: String,
    /// how full the context window is, 0..1
    pub ctx: f64,
}

#[derive(Clone, Serialize)]
pub struct Notification {
    /// the scrollback entry this banner belongs to (0 for a session brief-show)
    pub id: u64,
    pub title: String,
    pub body: String,
    /// what clicking the banner does ("viber" = open the Viber app); None = nothing
    pub action: Option<String>,
    /// set for a session brief-show (title is the session's title); not part of the scrollback
    pub brief: Option<Brief>,
}

impl Notification {
    /// how long it stays: a session that needs you stays longer than a banner
    fn dwell(&self) -> Duration {
        Duration::from_millis(match self.brief.as_ref().map(|b| b.state) {
            Some("waiting") => 8000,
            Some(_) => 5000,
            None => NOTIF_MS,
        })
    }
}

#[derive(Clone, Serialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// milliseconds since the Unix epoch -- the frontend formats it with
    /// `new Date(ms)` rather than this side rendering a locale-specific string
    pub time_ms: u64,
    pub action: Option<String>,
    /// how many messages this entry stands for (more than 1 once they were combined)
    pub count: u32,
}

#[derive(Default)]
pub struct NotifState {
    current: Option<Notification>,
    deadline: Option<Instant>,
    queue: VecDeque<Notification>,
    history: VecDeque<HistoryEntry>,
    next_id: u64,
}

impl NotifState {
    pub fn push(&mut self, title: String, body: String) {
        self.push_action(title, body, None);
    }

    pub fn push_action(&mut self, title: String, body: String, action: Option<String>) {
        self.next_id += 1;
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // an identical notification replaces the older one: the scrollback (and the queue of
        // banners still waiting to show) keeps only the latest, at the top
        self.history.retain(|e| !(e.title == title && e.body == body && e.action == action));
        self.queue.retain(|n| !(n.title == title && n.body == body && n.action == action));
        // too many from one chat app: they become one entry that counts them
        let (mut title, mut body, mut count) = (title, body, 1u32);
        if action.as_deref() == Some("discord") {
            let before: u32 = self.history.iter().filter(|e| e.action == action).map(|e| e.count).sum();
            if before + 1 > GROUP_AFTER {
                count = before + 1;
                self.history.retain(|e| e.action != action);
                self.queue.retain(|n| n.action != action);
                title = "Discord".into();
                body = format!("{count} new messages");
            }
        }
        self.history.push_front(HistoryEntry {
            id: self.next_id,
            title: title.clone(),
            body: body.clone(),
            time_ms,
            action: action.clone(),
            count,
        });
        self.history.truncate(HISTORY_CAP);
        self.queue.push_back(Notification { id: self.next_id, title, body, action, brief: None });
    }

    /// queue a session brief-show. One at a time, in line with the banners: a session that needs
    /// you goes to the front of the queue (not in front of what is showing), a finished one waits
    /// its turn. Never part of the scrollback.
    pub fn push_brief(&mut self, title: String, brief: Brief) {
        self.queue.retain(|n| n.brief.as_ref().map_or(true, |b| b.id != brief.id));
        let n = Notification { id: 0, title, body: String::new(), action: None, brief: Some(brief) };
        if n.brief.as_ref().map_or(false, |b| b.state == "waiting") {
            let at = self.queue.iter().position(|q| q.brief.as_ref().map_or(true, |b| b.state != "waiting")).unwrap_or(self.queue.len());
            self.queue.insert(at, n);
        } else {
            self.queue.push_back(n);
        }
    }

    /// the session does not need you any more (you answered): its brief goes away
    pub fn cancel_brief(&mut self, id: &str) {
        self.queue.retain(|n| n.brief.as_ref().map_or(true, |b| b.id != id));
        if self.current.as_ref().and_then(|n| n.brief.as_ref()).map_or(false, |b| b.id == id && b.state == "waiting") {
            self.dismiss_current();
        }
    }

    pub fn current_is_brief(&self) -> bool {
        self.current.as_ref().map_or(false, |n| n.brief.is_some())
    }

    /// end the banner that is showing right now (the next tick advances the queue)
    pub fn dismiss_current(&mut self) {
        if self.current.is_some() {
            self.deadline = Some(Instant::now());
        }
    }

    pub fn history(&self) -> Vec<HistoryEntry> {
        self.history.iter().cloned().collect()
    }

    /// drop one entry from the hub's scrollback (the user dismissed its card)
    pub fn remove(&mut self, id: u64) {
        self.history.retain(|e| e.id != id);
    }

    pub fn has_current(&self) -> bool {
        self.current.is_some()
    }

    /// Called every poll tick: advances the queue if the current banner's
    /// dwell time is up, or presents the first queued one if nothing is
    /// showing yet. Returns `Some(notification-or-none)` when the visible
    /// state just changed and the frontend needs telling, `None` if nothing
    /// changed this tick (the common case -- no need to re-emit every 16ms).
    pub fn tick(&mut self) -> Option<Option<Notification>> {
        let now = Instant::now();
        let expired = self.deadline.map(|d| now >= d).unwrap_or(false);
        if self.current.is_none() || expired {
            let next = self.queue.pop_front();
            let changed = next.is_some() || self.current.is_some();
            self.current = next.clone();
            self.deadline = next.as_ref().map(|n| now + n.dwell());
            if changed {
                return Some(next);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_notifications_keep_only_the_latest() {
        let mut n = NotifState::default();
        n.push("1 new notification".into(), "Phone Link".into());
        n.push("Mail".into(), "Hello".into());
        n.push("1 new notification".into(), "Phone Link".into());
        let h = n.history();
        assert_eq!(h.len(), 2);
        // the latest one is on top, and it is the newer entry
        assert_eq!(h[0].title, "1 new notification");
        assert!(h[0].id > h[1].id);
        // one banner waits for it, not two
        assert_eq!(n.queue.len(), 2);
    }

    #[test]
    fn a_session_that_needs_you_jumps_the_queue() {
        let b = |id: &str, state: &'static str| Brief { id: id.into(), state, host_icon: "code", host_exe: None, project: "p".into(), ctx: 0.1 };
        let mut n = NotifState::default();
        n.push("Mail".into(), "Hello".into());
        n.push_brief("done".into(), b("a", "finished"));
        n.push_brief("asks".into(), b("b", "waiting"));
        n.push_brief("asks too".into(), b("c", "waiting"));
        let order: Vec<_> = n.queue.iter().map(|q| q.title.clone()).collect();
        assert_eq!(order, ["asks", "asks too", "Mail", "done"]);
        // one brief per session, and it can be withdrawn
        n.push_brief("done again".into(), b("a", "finished"));
        n.cancel_brief("b");
        let order: Vec<_> = n.queue.iter().map(|q| q.title.clone()).collect();
        assert_eq!(order, ["asks too", "Mail", "done again"]);
        // briefs never reach the scrollback
        assert_eq!(n.history().len(), 1);
    }

    #[test]
    fn many_discord_messages_become_one() {
        let mut n = NotifState::default();
        for (who, text) in [("Ann", "hi"), ("Bo", "hello"), ("Cy", "hey")] {
            n.push_action(who.into(), text.into(), Some("discord".into()));
        }
        assert_eq!(n.history().len(), 3); // up to three stay as they are
        n.push_action("Di".into(), "yo".into(), Some("discord".into()));
        let h = n.history();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].title.as_str(), h[0].body.as_str(), h[0].count), ("Discord", "4 new messages", 4));
        assert_eq!(n.queue.len(), 1);
        n.push_action("Ed".into(), "sup".into(), Some("discord".into()));
        assert_eq!(n.history()[0].body, "5 new messages");
        // other apps are left alone
        n.push_action("Mail".into(), "x".into(), None);
        assert_eq!(n.history().len(), 2);
    }

    #[test]
    fn different_text_is_not_merged() {
        let mut n = NotifState::default();
        n.push("A".into(), "same".into());
        n.push("B".into(), "same".into());
        assert_eq!(n.history().len(), 2);
    }
}
