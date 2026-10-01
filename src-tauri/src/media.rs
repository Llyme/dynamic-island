//! Windows SMTC (System Media Transport Controls) media session bridge --
//! the Rust equivalent of `platform_backend/windows.py`'s `MediaBridge`.
//!
//! Unlike the Python version (which wires the SDK's own change-notification
//! events), this polls once a second on a dedicated OS thread. SMTC's WinRT
//! objects need a COM apartment initialized on whatever thread touches them,
//! and wiring `TypedEventHandler` callbacks across that boundary safely is a
//! lot of unsafe-adjacent plumbing for a value that's fine to be up to ~1s
//! stale. A plain thread + blocking calls is far simpler and just as
//! correct for this app's purposes (nothing here is a tight loop).

use crate::IslandState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

const POLL_MS: u64 = 1000;

#[derive(Serialize, Clone, Default)]
pub struct MediaSnapshot {
    pub has_session: bool,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub can_previous: bool,
    pub can_next: bool,
    /// the player takes seeks (jump 5 s, the seek bar)
    pub can_seek: bool,
    /// the player takes a playback speed
    pub can_rate: bool,
    /// playback speed, 1.0 = normal
    pub rate: f64,
    /// data: URL (PNG), ready to drop straight into an <img src>.
    pub art: Option<String>,
    /// the playing app's AppUserModelId (used to focus its window from the hub card)
    pub source: String,
}

fn current_session() -> windows::core::Result<Option<Session>> {
    let manager = SessionManager::RequestAsync()?.get()?;
    Ok(manager.GetCurrentSession().ok())
}

fn read_snapshot(session: &Session) -> windows::core::Result<Option<MediaSnapshot>> {
    let playback = session.GetPlaybackInfo()?;
    let status = playback.PlaybackStatus()?;
    if status == PlaybackStatus::Closed {
        return Ok(None);
    }

    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = props.Title().map(|h| h.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
    if title.is_empty() && artist.is_empty() {
        return Ok(None);
    }

    let (mut position, mut duration) = (0.0, 0.0);
    if let Ok(timeline) = session.GetTimelineProperties() {
        if let (Ok(pos), Ok(start), Ok(end)) = (
            timeline.Position(),
            timeline.StartTime(),
            timeline.EndTime(),
        ) {
            position = pos.Duration as f64 / 10_000_000.0;
            duration = (end.Duration - start.Duration) as f64 / 10_000_000.0;
        }
    }

    let rate = playback.PlaybackRate().ok().and_then(|r| r.Value().ok()).filter(|r| *r > 0.0).unwrap_or(1.0);
    let controls = playback.Controls().ok();
    let can_seek = controls.as_ref().and_then(|c| c.IsPlaybackPositionEnabled().ok()).unwrap_or(false);
    let can_rate = controls.as_ref().and_then(|c| c.IsPlaybackRateEnabled().ok()).unwrap_or(false);
    let can_previous = controls
        .as_ref()
        .and_then(|c| c.IsPreviousEnabled().ok())
        .unwrap_or(true);
    let can_next = controls
        .as_ref()
        .and_then(|c| c.IsNextEnabled().ok())
        .unwrap_or(true);

    let art = read_thumbnail(&props).ok().flatten();

    Ok(Some(MediaSnapshot {
        has_session: true,
        title,
        artist,
        playing: status == PlaybackStatus::Playing,
        position,
        duration,
        can_previous,
        can_next,
        can_seek,
        can_rate,
        rate,
        art,
        source: session
            .SourceAppUserModelId()
            .map(|h| h.to_string())
            .unwrap_or_default(),
    }))
}

fn read_thumbnail(
    props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> windows::core::Result<Option<String>> {
    use base64::Engine;
    use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};

    let Some(thumb) = props.Thumbnail().ok() else {
        return Ok(None);
    };
    let stream = thumb.OpenReadAsync()?.get()?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Ok(None);
    }
    // Sources vary (album art is usually a JPEG or PNG, browser tab
    // thumbnails can be either too) -- trust the stream's own reported
    // type instead of assuming PNG, or a non-PNG thumbnail just silently
    // fails to decode as a broken-image icon in the <img>.
    let content_type = stream.ContentType().ok().map(|h| h.to_string());
    let mime = content_type
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "image/png".to_string());
    let buffer = Buffer::Create(size)?;
    stream
        .ReadAsync(&buffer, size, InputStreamOptions::None)?
        .get()?;
    let reader = DataReader::FromBuffer(&buffer)?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(Some(format!("data:{mime};base64,{b64}")))
}

fn snapshot_now() -> MediaSnapshot {
    match current_session().and_then(|s| match s {
        Some(session) => read_snapshot(&session),
        None => Ok(None),
    }) {
        Ok(Some(snap)) => snap,
        Ok(None) | Err(_) => MediaSnapshot::default(),
    }
}

/// Spawns the poll thread; call once from `setup`.
pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut last_track = (String::new(), String::new());
        loop {
            let snap = snapshot_now();
            state.has_media.store(snap.has_session, Ordering::Relaxed);
            let track = (snap.title.clone(), snap.artist.clone());
            if snap.has_session && track != last_track {
                state.peek_request.store(true, Ordering::Relaxed);
            }
            last_track = track;
            // Emitted unconditionally (no change-detection dedup) -- a
            // dedup-by-content-hash version of this raced the frontend's
            // listener registration: the first snapshot could fire (and get
            // deduped against on every later identical tick) before
            // `main.js` had finished loading and calling `listen()`, silently
            // losing the session forever. One JSON emit/sec is cheap enough
            // to not need the optimization.
            let _ = app.emit("media-tick", snap);
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    });
}

fn with_current_session<F: FnOnce(&Session) -> windows::core::Result<()>>(f: F) {
    if let Ok(Some(session)) = current_session() {
        let _ = f(&session);
    }
}

#[tauri::command]
pub fn media_play_pause() {
    with_current_session(|s| s.TryTogglePlayPauseAsync()?.get().map(|_| ()));
}

#[tauri::command]
pub fn media_next() {
    with_current_session(|s| s.TrySkipNextAsync()?.get().map(|_| ()));
}

#[tauri::command]
pub fn media_previous() {
    with_current_session(|s| s.TrySkipPreviousAsync()?.get().map(|_| ()));
}

#[tauri::command]
pub fn media_seek(position_seconds: f64) {
    with_current_session(|s| {
        let ticks = (position_seconds * 10_000_000.0) as i64;
        s.TryChangePlaybackPositionAsync(ticks)?.get().map(|_| ())
    });
}

/// jump forward (positive) or back (negative) by some seconds, from where the player is now:
/// the timeline's position is the one it last reported, so the time since then is added (at
/// the playback speed) while it plays
#[tauri::command]
pub fn media_seek_by(delta_seconds: f64) {
    with_current_session(|s| {
        let tl = s.GetTimelineProperties()?;
        let playback = s.GetPlaybackInfo()?;
        let playing = playback.PlaybackStatus()? == PlaybackStatus::Playing;
        let rate = playback.PlaybackRate().ok().and_then(|r| r.Value().ok()).filter(|r| *r > 0.0).unwrap_or(1.0);
        let start = tl.StartTime()?.Duration as f64 / 10_000_000.0;
        let end = tl.EndTime()?.Duration as f64 / 10_000_000.0;
        let mut pos = tl.Position()?.Duration as f64 / 10_000_000.0;
        if playing {
            // `LastUpdatedTime` is a Windows FILETIME-style count of 100 ns since 1601
            let updated = tl.LastUpdatedTime()?.UniversalTime as f64 / 10_000_000.0;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) + 11_644_473_600.0;
            let since = (now - updated).clamp(0.0, 30.0);
            pos += since * rate;
        }
        let target = (pos + delta_seconds).clamp(start, if end > start { end } else { f64::MAX });
        s.TryChangePlaybackPositionAsync((target * 10_000_000.0) as i64)?.get().map(|_| ())
    });
}

/// set the playback speed (0.1 to 16); true when the player took it
#[tauri::command]
pub fn media_set_rate(rate: f64) -> bool {
    let rate = rate.clamp(0.1, 16.0);
    let mut ok = false;
    with_current_session(|s| {
        ok = s.TryChangePlaybackRateAsync(rate)?.get()?;
        Ok(())
    });
    ok
}

