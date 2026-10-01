mod activity;
mod audio;
mod background;
mod calendar;
mod exeinfo;
mod focus;
mod game;
mod gpu;
mod media;
mod clock;
mod notify;
mod browse;
mod downloads;
mod fps;
mod gamestats;
mod llm;
mod pagetext;
mod project;
mod scan;
mod notify_listener;
mod settings;
mod stats;
mod usage;
mod winutil;
mod work;

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{
    tray::TrayIconBuilder, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewWindow,
};

const EDGE_TRIGGER_PX: i32 = 3;
const EDGE_POLL_MS: u64 = 16;
const HIDDEN_OFFSET: i32 = 200; // how far above the monitor top the pill parks while hidden
const PEEK_MARGIN: i32 = 4; // how far from the monitor's side edges the pill keeps (its distance from the top is a setting)
const HIDE_SECS: f64 = 0.45; // length of the slide-away animation
const NUDGE_EASE: f64 = 0.16; // per-tick ease for horizontal cursor-follow while shown (a live target, not a spring-to-rest animation)

// the window is this much larger than the island on every side (transparent, and click-through
// away from the island itself) so the glow of a brief-show view can shine outside the island
const GLOW_PAD: f64 = 44.0;

// edge dwell: while the cursor rests at the top edge the (still hidden) island window is put
// where the island will stand, click-through and with the island itself out of sight, so a glow
// can build up at the screen's edge before the island lands there. The window never changes
// size or place when the island appears -- only what is drawn in it does -- so nothing can twitch.
// after the glow is fully charged the island lands (a CSS animation) while the glow drains;
// REVEAL_SECS is how long that lasts
const REVEAL_SECS: f64 = 0.6;

/// the width of the standard collapsed island (the media and work pills) before the setting
const COMPACT_BASE: f64 = 260.0;
const IDLE_SIZE: (f64, f64) = (140.0, 28.0);
const MEDIA_COMPACT_SIZE: (f64, f64) = (260.0, 40.0);
const GAME_SIZE: (f64, f64) = (320.0, 40.0);
const WORK_SIZE: (f64, f64) = (260.0, 40.0);
// the notification banner and the session pill take the width of the collapsed island they drop over
const NOTIF_SIZE: (f64, f64) = (IDLE_SIZE.0, 72.0); // (the width follows the collapsed view, see view_size)
const USAGE_PEEK_SIZE: (f64, f64) = (240.0, 78.0);
const BRIEF_SIZE: (f64, f64) = (IDLE_SIZE.0, 40.0);
// settle (bars sit at the old value) + fill animation + hold, ms
pub(crate) const USAGE_PEEK_TOTAL_MS: u64 = 500 + 1400 + 2500;
// the hub is as tall as its content, between the two heights, and as wide as the setting says
// (this is only the default)
const HUB_WIDTH: f64 = 420.0;
const HUB_MIN_H: f64 = 150.0;
const HUB_MAX_H: f64 = 520.0;
const HUB_SIZE: (f64, f64) = (HUB_WIDTH, HUB_MAX_H);

#[derive(PartialEq, Clone, Copy)]
enum PillView {
    Idle,
    Media,
    Game,
    Work,
    Notification,
    UsagePeek,
    /// a Claude Code session that needs you or finished: a compact pill from the notification queue
    Brief,
    Hub,
}

/// the size of a view. `width` is the setting for the standard collapsed island (px): the media and
/// work pills, the notification banner and the session pill are that wide, the others (idle, game,
/// usage peek) keep their proportions to it.
fn view_size(view: PillView, width: u64, hub_w: f64) -> (f64, f64) {
    let (w, h) = view.size();
    let k = width as f64 / COMPACT_BASE;
    match view {
        PillView::Hub => (hub_w, h),
        PillView::Notification | PillView::Brief => (width as f64, h),
        _ => (w * k, h),
    }
}

impl PillView {
    fn size(self) -> (f64, f64) {
        match self {
            PillView::Idle => IDLE_SIZE,
            PillView::Media => MEDIA_COMPACT_SIZE,
            PillView::Game => GAME_SIZE,
            PillView::Work => WORK_SIZE,
            PillView::Notification => NOTIF_SIZE,
            PillView::UsagePeek => USAGE_PEEK_SIZE,
            PillView::Brief => BRIEF_SIZE,
            PillView::Hub => HUB_SIZE,
        }
    }

    fn js_name(self) -> &'static str {
        match self {
            PillView::Idle => "idle",
            PillView::Media => "media",
            PillView::Game => "game",
            PillView::Work => "work",
            PillView::Notification => "notification",
            PillView::UsagePeek => "usage_peek",
            PillView::Brief => "brief",
            PillView::Hub => "hub",
        }
    }
}

#[derive(Serialize, Clone)]
struct CursorTick {
    /// Cursor position local to the window, in CSS pixels -- what the
    /// eyes' gaze-tracking and squash/stretch need.
    local_x: f64,
    local_y: f64,
    shown: bool,
    /// pinned and left alone: the island is drawn smaller, down to `shrink` percent
    shrunk: bool,
    shrink: u64,
    /// 0..1 while the cursor is dwelling at the top edge before the reveal
    charge: f64,
    /// the window is enlarged for the glow (charging, and while the island lands inside it)
    big: bool,
    /// margin between the window's edge and the island, CSS px (room for the glow)
    pad: f64,
    /// distance from the window's top to the screen's top edge, CSS px: where the glow is anchored
    edge: f64,
    /// where the island's top sits inside the enlarged window, CSS px
    pill_top: f64,
    /// size the island itself has in the current view, CSS px (the window is bigger while `big`)
    pill_w: f64,
    pill_h: f64,
}

/// Shared, lock-free state the 60fps-ish poll loop, the media poll thread,
/// and the drag commands all touch. Plain atomics are enough here --
/// nothing but simple ints/bools crosses this boundary. Hover is
/// deliberately *not* tracked via webview mouseenter/leave events: those go
/// through WebView2's own input pipeline and lag behind (or miss entirely)
/// a window that's simultaneously being repositioned every tick by this
/// same loop. The poll loop instead computes hover itself each tick from
/// the cursor position vs. the window's own last-set rect, which is exact
/// and can't desync.
pub(crate) struct IslandState {
    dragging: AtomicBool,
    shown: AtomicBool,
    pub(crate) has_media: AtomicBool,
    pub(crate) has_game: AtomicBool,
    pub(crate) has_work: AtomicBool,
    pub(crate) notif: std::sync::Mutex<notify::NotifState>,
    /// The pill is expanded into the hub panel -- toggled by clicking the
    /// pill, outranks every other view while true (matches the Qt version's
    /// `open_hub`/`close_hub`).
    hub_open: AtomicBool,
    /// logical height the frontend wants the expanded hub to have (fits its content)
    hub_height: AtomicU32,
    /// Window position when the current drag gesture's mousedown fired --
    /// compared against the position at mouseup to tell a real drag apart
    /// from a plain click (which also goes through `start_dragging`, since
    /// that's the simplest way to get native OS window-move behavior; a
    /// click just ends the move loop having gone nowhere).
    drag_start_pos: std::sync::Mutex<Option<(i32, i32)>>,
    /// cursor_x - window_x at the moment a drag gesture grabbed the pill --
    /// kept constant through the drag so the pill doesn't jump to be
    /// centered under the cursor, it just follows wherever it was grabbed.
    drag_grab_dx: AtomicI32,
    /// Screen-space X the pill is centered on when idle (no drag/follow in
    /// progress) -- physical pixels.
    center_x: AtomicI32,
    pub(crate) settings: std::sync::Mutex<settings::Settings>,
    pub(crate) usage: usage::UsageState,
    pub(crate) calendar: calendar::CalendarState,
    pub(crate) activity: activity::ActivityState,
    pub(crate) gpu: gpu::GpuState,
    pub(crate) game_meter: std::sync::Mutex<gamestats::GameMeter>,
    pub(crate) sys: std::sync::Mutex<sysinfo::System>,
    pub(crate) game_detection_enabled: AtomicBool,
    pub(crate) work_detection_enabled: AtomicBool,
    /// live-editable copy of IDLE_HIDE_MS -- settings can change this
    /// without a restart
    idle_hide_ms: AtomicU64,
    /// while in the future, the "Claude usage changed" peek is showing
    pub(crate) usage_peek_until: std::sync::Mutex<Option<Instant>>,
    /// how long a media/game/work view stays up before sliding away again
    peek_ms: AtomicU64,
    /// how long the cursor must rest at the top edge before the island appears (0 = at once)
    edge_dwell_ms: AtomicU64,
    /// size (percent) a pinned, untouched island shrinks to; 100 = it never shrinks
    pin_shrink: AtomicU64,
    /// width (percent) of the collapsed island; the hub keeps its own width
    compact_width: AtomicU64,
    /// width (logical px) of the expanded island
    hub_width: AtomicU64,
    /// how far below the top edge (logical px) the island sits while shown
    top_margin: AtomicU64,
    show_at_cursor: AtomicBool,
    /// right-click pin: the island stays put (no idle-hide, no dash to another
    /// monitor, hub survives click-outside) until right-clicked again
    pinned: AtomicBool,
    pub(crate) audio_enabled: AtomicBool,
    cursor_follow: AtomicBool,
    /// set by the media thread on a track change: re-present the pill for a
    /// fresh peek even though the view itself didn't change
    pub(crate) peek_request: AtomicBool,
}

impl IslandState {
    pub(crate) fn hub_is_open(&self) -> bool {
        self.hub_open.load(Ordering::Relaxed)
    }
}

impl Default for IslandState {
    fn default() -> Self {
        let loaded = settings::load();
        Self {
            dragging: AtomicBool::new(false),
            shown: AtomicBool::new(false),
            has_media: AtomicBool::new(false),
            has_game: AtomicBool::new(false),
            has_work: AtomicBool::new(false),
            notif: std::sync::Mutex::new(notify::NotifState::default()),
            hub_open: AtomicBool::new(false),
            hub_height: AtomicU32::new(HUB_MAX_H as u32),
            drag_start_pos: std::sync::Mutex::new(None),
            drag_grab_dx: AtomicI32::new(0),
            center_x: AtomicI32::new(0),
            usage: usage::UsageState::default(),
            calendar: calendar::CalendarState::default(),
            activity: activity::ActivityState::default(),
            gpu: gpu::GpuState::default(),
            game_meter: std::sync::Mutex::new(gamestats::GameMeter::default()),
            sys: std::sync::Mutex::new(stats::new_system()),
            game_detection_enabled: AtomicBool::new(loaded.game_detection),
            work_detection_enabled: AtomicBool::new(loaded.work_detection),
            idle_hide_ms: AtomicU64::new(loaded.idle_hide_delay_s.max(1) * 1000),
            peek_ms: AtomicU64::new(loaded.peek_duration_s.max(1) * 1000),
            edge_dwell_ms: AtomicU64::new(loaded.edge_dwell_ms),
            pin_shrink: AtomicU64::new(loaded.pin_shrink.clamp(30, 100)),
            compact_width: AtomicU64::new(settings::compact_px(loaded.compact_width)),
            hub_width: AtomicU64::new(loaded.hub_width.clamp(340, 640)),
            top_margin: AtomicU64::new(loaded.top_margin.clamp(0, 80)),
            peek_request: AtomicBool::new(false),
            show_at_cursor: AtomicBool::new(loaded.show_at_cursor),
            pinned: AtomicBool::new(false),
            audio_enabled: AtomicBool::new(loaded.react_to_audio),
            cursor_follow: AtomicBool::new(loaded.cursor_follow),
            usage_peek_until: std::sync::Mutex::new(None),
            settings: std::sync::Mutex::new(loaded),
        }
    }
}

#[tauri::command]
fn toggle_hub(window: WebviewWindow) {
    let state = window.state::<Arc<IslandState>>();
    let now_open = !state.hub_open.load(Ordering::Relaxed);
    state.hub_open.store(now_open, Ordering::Relaxed);
    if now_open {
        state.shown.store(true, Ordering::Relaxed);
    }
}

fn open_hub_from_tray(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("island") {
        let state = w.state::<Arc<IslandState>>();
        state.hub_open.store(true, Ordering::Relaxed);
        state.shown.store(true, Ordering::Relaxed);
        // focus so the click-outside-closes-hub blur handler has something to fire on
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn toggle_pin(window: WebviewWindow) {
    let state = window.state::<Arc<IslandState>>();
    let now_pinned = !state.pinned.load(Ordering::Relaxed);
    state.pinned.store(now_pinned, Ordering::Relaxed);
    if now_pinned {
        state.shown.store(true, Ordering::Relaxed);
    }
    let _ = window.emit("pin-tick", now_pinned);
}

#[tauri::command]
fn set_hub_height(window: WebviewWindow, h: f64) {
    let h = h.clamp(HUB_MIN_H, HUB_MAX_H).round() as u32;
    window.state::<Arc<IslandState>>().hub_height.store(h, Ordering::Relaxed);
}

#[tauri::command]
fn close_hub(window: WebviewWindow) {
    window.state::<Arc<IslandState>>().hub_open.store(false, Ordering::Relaxed);
}

#[tauri::command]
fn push_notification(window: WebviewWindow, title: String, body: String) {
    let state = window.state::<Arc<IslandState>>();
    let history = {
        let mut notif = state.notif.lock().unwrap();
        notif.push(title, body);
        notif.history()
    };
    let _ = window.emit("notification-history-tick", history);
}

/// A banner (or history row) was clicked: run its action and drop the banner.
#[tauri::command]
fn open_notification_action(window: WebviewWindow, action: String) {
    match action.as_str() {
        "viber" => notify_listener::open_viber(),
        "discord" => focus::focus_or_open("discord.exe", windows::core::w!("discord://")),
        _ => {}
    }
    window.state::<Arc<IslandState>>().notif.lock().unwrap().dismiss_current();
}

/// The banner on screen was clicked: run its action (if it has one) and be done with it, both the
/// banner and its entry in the hub's scrollback.
#[tauri::command]
fn click_notification(window: WebviewWindow, id: u64, action: Option<String>) {
    if let Some(action) = action {
        open_notification_action(window.clone(), action);
    }
    let state = window.state::<Arc<IslandState>>();
    let history = {
        let mut notif = state.notif.lock().unwrap();
        notif.remove(id);
        notif.dismiss_current();
        notif.history()
    };
    let _ = window.emit("notification-history-tick", history);
}

#[tauri::command]
fn dismiss_notification(window: WebviewWindow, id: u64) {
    let state = window.state::<Arc<IslandState>>();
    let history = {
        let mut notif = state.notif.lock().unwrap();
        notif.remove(id);
        notif.history()
    };
    let _ = window.emit("notification-history-tick", history);
}

#[tauri::command]
fn get_notification_history(window: WebviewWindow) -> Vec<notify::HistoryEntry> {
    let state = window.state::<Arc<IslandState>>();
    let history = state.notif.lock().unwrap().history();
    history
}

/// Manual drag instead of `start_dragging()`'s native OS move-loop -- two
/// reasons: that loop can swallow the DOM mouseup a click needs to detect
/// (see the removed `drag_end` command's old comment), and it moves the
/// window freely in both axes, while the pill should only ever slide
/// horizontally (same as the Qt version's `self.move(new_x, self.y())`,
/// which never touched y during a drag).
#[tauri::command]
fn drag_start(window: WebviewWindow) {
    let state = window.state::<Arc<IslandState>>();
    state.dragging.store(true, Ordering::Relaxed);
    if let Ok(pos) = window.outer_position() {
        // (the island sits GLOW_PAD inside the window)
        let pad = (GLOW_PAD * window.scale_factor().unwrap_or(1.0)).round() as i32;
        let (px, py) = (pos.x + pad, pos.y + pad);
        *state.drag_start_pos.lock().unwrap() = Some((px, py));
        let (cx, _cy) = winutil::cursor_pos();
        state.drag_grab_dx.store(cx - px, Ordering::Relaxed);
    }
}

// There is deliberately no `drag_end` command: `start_dragging()`'s native
// OS move-loop can swallow the DOM `mouseup` the webview would otherwise
// see, so end-of-drag (and the click-vs-drag decision that depends on it)
// is detected by the edge-poll loop instead, by polling the left mouse
// button's real state -- see `spawn_edge_poll`.

/// Put the window where the island (`x`,`y`,`w`,`h`, physical px) belongs, with `pad` of
/// margin all round, in a single call -- and only when something changed.
fn place_window(hwnd: isize, last: &mut (i32, i32, i32, i32), x: f64, y: f64, w: f64, h: f64, pad: f64) {
    let rect = (
        (x - pad).round() as i32,
        (y - pad).round() as i32,
        (w + 2.0 * pad).round() as i32,
        (h + 2.0 * pad).round() as i32,
    );
    if rect != *last {
        *last = rect;
        winutil::place(hwnd, rect.0, rect.1, rect.2, rect.3);
    }
}

/// Switch off what makes the webview behave like a browser: the accelerator keys (F5, Ctrl+R,
/// Ctrl+P, Ctrl+F, F12...), devtools, zoom, the default context menu, swipe navigation, the status
/// bar and autofill. Editing keys (copy, paste, select all) still work in the text fields.
fn lock_down_webview(window: &WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
    use windows_core::Interface;
    let _ = window.with_webview(|wv| unsafe {
        let Ok(core) = wv.controller().CoreWebView2() else { return };
        let Ok(settings) = core.Settings() else { return };
        let _ = settings.SetAreDefaultContextMenusEnabled(false);
        let _ = settings.SetAreDevToolsEnabled(false);
        let _ = settings.SetIsZoomControlEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        let _ = settings.SetIsBuiltInErrorPageEnabled(false);
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
        }
    });
}

fn lerp(current: f64, target: f64, ease: f64) -> f64 {
    current + (target - current) * ease
}

// -- cartoon-springy motion, used everywhere something animates to a target
// (show/hide slide, the monitor-to-monitor dash, expanding/collapsing the
// pill) instead of a flat ease-toward-target: a damped harmonic oscillator
// slightly underdamped so it overshoots and settles, the same "squash and
// bounce" feel the eyes' squash/stretch already has, rather than every
// transition gliding to a dead stop.
const SPRING_STIFFNESS: f64 = 1300.0;
const SPRING_DAMPING: f64 = 48.0; // same ~0.62 damping ratio as before, just faster (higher stiffness) -- settles in ~170ms instead of ~440ms
const SPRING_SETTLE_DIST: f64 = 0.4;
const SPRING_SETTLE_VEL: f64 = 2.0;

/// One tick of spring physics toward `target`. `vel` is the spring's
/// velocity, carried by the caller across ticks (in the same units as
/// `value` per second). Returns the new value; `vel` is updated in place.
fn spring_step(value: f64, target: f64, vel: &mut f64, dt: f64) -> f64 {
    let accel = -SPRING_STIFFNESS * (value - target) - SPRING_DAMPING * *vel;
    *vel += accel * dt;
    value + *vel * dt
}

/// Ease for the hide slide: a small anticipation dip first (the pill sinks a
/// few pixels), then it accelerates up and out -- reads as a deliberate
/// "tuck away" instead of the spring's fast-start that looked like a cut.
fn ease_in_back(p: f64) -> f64 {
    let c1 = 1.5;
    (c1 + 1.0) * p * p * p - c1 * p * p
}

fn ease_in_out(p: f64) -> f64 {
    p * p * (3.0 - 2.0 * p)
}

fn spring_settled(value: f64, target: f64, vel: f64) -> bool {
    (value - target).abs() < SPRING_SETTLE_DIST && vel.abs() < SPRING_SETTLE_VEL
}

/// The whole show/hide/follow/resize state machine, ticking on its own
/// timer instead of reacting to webview mouse events -- Windows only
/// delivers mouse events to a window while the cursor is over it, but the
/// edge-hover trigger needs the cursor position everywhere on screen, same
/// reason the Python/Qt version polled `QCursor.pos()` globally instead of
/// listening for real mouse events.
///
/// Window size is tracked locally (`win_w`/`win_h`) rather than re-queried
/// from the OS every tick -- it only ever changes when this loop itself
/// calls `set_size` (on a view switch), so there's no need to pay for an
/// IPC round-trip to ask the OS something this loop already knows.
fn spawn_edge_poll(window: WebviewWindow) {
    tauri::async_runtime::spawn(async move {
        let state = window.state::<Arc<IslandState>>().inner().clone();
        let scale = window.scale_factor().unwrap_or(1.0);
        let (mut win_w, mut win_h) = window
            .outer_size()
            .map(|s| (s.width as f64, s.height as f64))
            .unwrap_or((IDLE_SIZE.0 * scale, IDLE_SIZE.1 * scale));
        // `setup()` already placed the window (centered, parked off-screen
        // above the monitor) before this loop starts -- read that back
        // instead of assuming x=0, which put a media session's first resize
        // off the left edge of the screen (centering math anchored on a
        // fictitious x=0 starting point instead of where the window actually is)
        let (mut pos_x, mut pos_y) = window
            .outer_position()
            .map(|p| (p.x as f64, p.y as f64))
            .unwrap_or((0.0, -(HIDDEN_OFFSET as f64)));
        // the window is the island plus a margin for the glow; pos_x/pos_y/win_w/win_h describe the island
        let pad_full = (GLOW_PAD * scale).round();
        pos_x += pad_full;
        pos_y += pad_full;
        let mut applied_width = state.compact_width.load(Ordering::Relaxed);
        win_w = IDLE_SIZE.0 * scale * applied_width as f64 / COMPACT_BASE;
        win_h = IDLE_SIZE.1 * scale;
        // the window setup() parked was sized for the plain 140 px pill, without the margin or the
        // width setting: centre the island itself on the screen instead of trusting that position
        let screen_mid = state.center_x.load(Ordering::Relaxed);
        if screen_mid != 0 {
            pos_x = screen_mid as f64 - win_w / 2.0;
        }
        let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        winutil::fine_timer();
        let mut placed = (i32::MIN, 0, 0, 0);
        let mut ignoring = false;
        // ticks the island has been pinned and left alone (cursor away, not dragged)
        let mut pin_idle_ticks: u64 = 0;
        let mut idle_ticks: u64 = 0;
        let mut current_view = PillView::Idle;
        // which monitor (its geo.x) a pinned pill currently considers
        // itself anchored to -- MIN so the first pinned tick always centers
        // it fresh, same as the old `_present_pill` behavior
        let mut pill_monitor_x = i32::MIN;
        // right-click pin: the monitor the pill sat on when pinned -- everything
        // (clamping, y, hide logic) keeps using it, ignoring where the cursor goes
        let mut was_pinned = false;
        let mut pin_geo: Option<winutil::MonitorGeometry> = None;
        // Some(target x) while a pinned pill is dashing over to a monitor
        // it just detected the cursor moved to -- None the rest of the
        // time, including right after a manual drag, so that doesn't get
        // fought by this (see the monitor-change check below, and drag-end
        // syncing `pill_monitor_x` so it never triggers one right after)
        let mut pinned_dash_target: Option<f64> = None;
        // set once a dash to another monitor has centered the pill; stops the
        // edge-dwell cursor follow from pulling it off-center again
        let mut dashed_to_center = false;
        // spring velocities, carried across ticks -- one per animated
        // quantity (see spring_step's doc comment)
        let mut vel_y = 0.0; // show/hide slide
        let mut vel_dash_x = 0.0; // monitor-to-monitor dash
        let mut vel_w = 0.0; // view-switch resize (width)
        let mut vel_h = 0.0; // view-switch resize (height)
        // Some((target_w, target_h, anchor_mid_x)) while a view-switch
        // resize is springing toward its new size -- the pill grows/shrinks
        // from its current center (`anchor_mid_x`, captured once when the
        // resize starts) rather than the final target's center, so it
        // visibly morphs in place instead of jumping sideways mid-animation
        let mut size_anim: Option<(f64, f64, f64)> = None;
        // Some((elapsed s, from_y, to_y, from_x, to_x)) while the pill is
        // sliding away: up off the top edge and over to its monitor's center
        let mut hide_anim: Option<(f64, f64, f64, f64, f64)> = None;
        // the hub height (logical px) the current size animation was aimed at
        let mut applied_hub_h = HUB_MAX_H;
        let mut applied_hub_w = state.hub_width.load(Ordering::Relaxed) as f64;
        // edge dwell: seconds the cursor has rested at the top edge, and whether
        // the window is currently held at the edge for the glow
        let mut charge_t = 0.0_f64;
        let mut charge_big = false;
        let mut revealing = false;
        // whether the island was brought up by the cursor at the top edge (then the idle hide
        // delay applies) or by itself for a status peek (then the peek duration applies)
        let mut edge_revealed = false;
        let mut reveal_t = 0.0_f64;
        let dt = EDGE_POLL_MS as f64 / 1000.0;
        // Windows sleeps in ~16 ms steps, so a "16 ms" tick can really take 31 ms: the springs
        // are tuned to the nominal dt, but every user-facing delay must follow the real clock
        let mut last_tick = Instant::now();
        // when the window was last put back on top of the other topmost windows
        let mut last_raise = Instant::now();
        let mut was_up = false;

        loop {
            tokio::time::sleep(Duration::from_millis(EDGE_POLL_MS)).await;
            let real_dt = last_tick.elapsed().as_secs_f64().min(0.1);
            last_tick = Instant::now();
            let wpct = state.compact_width.load(Ordering::Relaxed);
            let hub_w = state.hub_width.load(Ordering::Relaxed) as f64;
            // distance from the top edge, physical px
            let top_px = (state.top_margin.load(Ordering::Relaxed) as f64 * scale).round() as i32;
            let (cx, cy) = winutil::cursor_pos();
            let geo = winutil::monitor_geometry_at(cx, cy);
            let Some(cursor_geo) = geo else { continue };
            // (named user_pin: `pinned` further down already means "view that parks the pill")
            let user_pin = state.pinned.load(Ordering::Relaxed);
            if user_pin && !was_pinned {
                pin_geo = winutil::monitor_geometry_at(
                    (pos_x + win_w / 2.0) as i32,
                    (pos_y + win_h / 2.0) as i32,
                );
            }
            was_pinned = user_pin;
            let geo = if user_pin { pin_geo.unwrap_or(cursor_geo) } else { cursor_geo };

            // end-of-drag detection: polled here (left mouse button's real
            // state) rather than via a `drag_end` command/DOM `mouseup`,
            // which `start_dragging()`'s native move-loop can swallow --
            // see the comment on the (removed) `drag_end` command
            if state.dragging.load(Ordering::Relaxed) && !winutil::left_button_down() {
                state.dragging.store(false, Ordering::Relaxed);
                if let Ok(real_pos) = window.outer_position() {
                    pos_x = real_pos.x as f64 + pad_full;
                    pos_y = real_pos.y as f64 + pad_full;
                }
                let half_w = win_w / 2.0;
                state.center_x.store((pos_x + half_w) as i32, Ordering::Relaxed);
                // wherever it was just dropped counts as "anchored" to that
                // monitor already -- don't immediately pull a pinned pill
                // back to that monitor's center right after the user placed
                // it somewhere specific
                pill_monitor_x = geo.x;
                pinned_dash_target = None;

                let start = state.drag_start_pos.lock().unwrap().take();
                let moved = start
                    .map(|(sx, sy)| (sx - pos_x as i32).abs() > 3 || (sy - pos_y as i32).abs() > 3)
                    .unwrap_or(false);
                if !moved {
                    // no real movement -- this was a click, not a drag: toggle the hub
                    let now_open = !state.hub_open.load(Ordering::Relaxed);
                    state.hub_open.store(now_open, Ordering::Relaxed);
                    if now_open {
                        state.shown.store(true, Ordering::Relaxed);
                    }
                }
            }

            // notifications get their own dwell timer (independent of the
            // idle-hide one below) and outrank every other view while one's
            // showing -- advancing the queue here, once a tick, is cheap
            // enough not to need its own thread
            if let Some(next) = state.notif.lock().unwrap().tick() {
                let _ = window.emit("notification-tick", &next);
            }
            let (has_notification, is_brief) = {
                let n = state.notif.lock().unwrap();
                (n.has_current(), n.current_is_brief())
            };

            // -- view switch: resize (keeping the pill horizontally centered
            // on its current midpoint) and force-present on a fresh media/
            // game/notification, same as the Qt version's `_present_pill`.
            // Priority: hub (explicit user action) > notification > media >
            // game > work, same order the Qt version used --
            let want_view = if state.hub_open.load(Ordering::Relaxed) {
                PillView::Hub
            } else if has_notification {
                if is_brief {
                    PillView::Brief
                } else {
                    PillView::Notification
                }
            } else if state
                .usage_peek_until
                .lock()
                .unwrap()
                .map_or(false, |t| Instant::now() < t)
            {
                PillView::UsagePeek
            } else if state.has_media.load(Ordering::Relaxed) {
                PillView::Media
            } else if state.has_game.load(Ordering::Relaxed) {
                PillView::Game
            } else if state.has_work.load(Ordering::Relaxed) {
                PillView::Work
            } else {
                PillView::Idle
            };
            if want_view != current_view {
                current_view = want_view;
                let (lw, mut lh) = view_size(want_view, wpct, hub_w);
                if want_view == PillView::Hub {
                    lh = state.hub_height.load(Ordering::Relaxed) as f64;
                    applied_hub_h = lh;
                }
                let anchor_mid_x = pos_x + win_w / 2.0;
                size_anim = Some((lw * scale, lh * scale, anchor_mid_x));
                vel_w = 0.0;
                vel_h = 0.0;
                let _ = window.emit("view-tick", want_view.js_name());
                if want_view != PillView::Idle {
                    state.shown.store(true, Ordering::Relaxed);
                    edge_revealed = false;
                    idle_ticks = 0;
                }
            }

            // the width setting changed: the collapsed island springs to its new width
            if wpct != applied_width {
                applied_width = wpct;
                if current_view != PillView::Hub && !charge_big {
                    let (lw, lh) = view_size(current_view, wpct, hub_w);
                    size_anim = Some((lw * scale, lh * scale, pos_x + win_w / 2.0));
                    vel_w = 0.0;
                    vel_h = 0.0;
                }
            }

            // hub open and its content grew/shrank: re-aim the height spring
            // (keeps the current velocity, so it just eases to the new height)
            if current_view == PillView::Hub {
                let want_h = state.hub_height.load(Ordering::Relaxed) as f64;
                if (want_h - applied_hub_h).abs() >= 1.0 || (hub_w - applied_hub_w).abs() >= 1.0 {
                    applied_hub_h = want_h;
                    applied_hub_w = hub_w;
                    let anchor = size_anim.map_or(pos_x + win_w / 2.0, |a| a.2);
                    size_anim = Some((hub_w * scale, want_h * scale, anchor));
                }
            }

            // spring the pill's size toward whatever view-switch target is
            // pending, growing/shrinking from its own center each tick
            // (`anchor_mid_x`) instead of instantly snapping -- the
            // cartoonish overshoot-and-settle "expanding" motion
            if let Some((target_w, target_h, anchor_mid_x)) = size_anim {
                win_w = spring_step(win_w, target_w, &mut vel_w, dt);
                win_h = spring_step(win_h, target_h, &mut vel_h, dt);
                let settled = spring_settled(win_w, target_w, vel_w)
                    && spring_settled(win_h, target_h, vel_h);
                if settled {
                    win_w = target_w;
                    win_h = target_h;
                    size_anim = None;
                }
                // grow around the midpoint, but never past the monitor's side
                // gap -- a pill parked at an edge slides inward as it widens
                pos_x = anchor_mid_x - win_w / 2.0;
                if pinned_dash_target.is_none() {
                    // (not mid-dash: `geo` is already the destination monitor,
                    // clamping to it would teleport the pill across)
                    pos_x = pos_x
                        .max((geo.x + PEEK_MARGIN) as f64)
                        .min((geo.x + geo.width - PEEK_MARGIN) as f64 - win_w);
                }
            }

            // fresh track etc. -- re-present for another peek
            if state.peek_request.swap(false, Ordering::Relaxed)
                && matches!(current_view, PillView::Media | PillView::Game | PillView::Work)
            {
                state.shown.store(true, Ordering::Relaxed);
                edge_revealed = false;
                idle_ticks = 0;
            }

            // -- edge dwell: resting at the top edge "calls" the island; a glow builds
            // there while the timer runs, and only then does it appear
            let dwell_s = state.edge_dwell_ms.load(Ordering::Relaxed) as f64 / 1000.0;
            let want_charge = dwell_s > 0.0
                && !state.shown.load(Ordering::Relaxed)
                && !state.dragging.load(Ordering::Relaxed)
                && cy <= geo.y + EDGE_TRIGGER_PX
                && hide_anim.is_none()
                && size_anim.is_none();
            let mut charge_ready = false;
            if want_charge {
                if !charge_big {
                    let (lw, lh) = view_size(current_view, wpct, hub_w);
                    let mid = if state.show_at_cursor.load(Ordering::Relaxed) {
                        cx as f64
                    } else {
                        geo.x as f64 + geo.width as f64 / 2.0
                    };
                    win_w = lw * scale;
                    win_h = lh * scale;
                    pos_x = (mid - win_w / 2.0)
                        .max((geo.x + PEEK_MARGIN) as f64)
                        .min((geo.x + geo.width - PEEK_MARGIN) as f64 - win_w);
                    // the island is still out of sight (hidden), so the window can jump there
                    pos_y = (geo.y + top_px) as f64;
                    vel_y = 0.0;
                    charge_big = true;
                }
                charge_t += real_dt;
                if state.show_at_cursor.load(Ordering::Relaxed) {
                    let want = (cx as f64 - win_w / 2.0)
                        .max((geo.x + PEEK_MARGIN) as f64)
                        .min((geo.x + geo.width - PEEK_MARGIN) as f64 - win_w);
                    pos_x = lerp(pos_x, want, 0.25);
                }
                charge_ready = charge_t >= dwell_s;
            } else if charge_t > 0.0 {
                // cursor left early: the glow drains away
                charge_t = (charge_t - real_dt * 4.0 * dwell_s.max(0.05)).max(0.0);
            }
            if charge_ready && charge_big && !revealing {
                // fully charged: the island appears now, inside the enlarged window, while the
                // glow's drain plays out at the same time
                revealing = true;
                reveal_t = 0.0;
                charge_t = 0.0;
            }
            if revealing {
                reveal_t += real_dt;
            }
            let reveal_done = revealing && reveal_t >= REVEAL_SECS;
            if charge_big
                && (reveal_done
                    || (!revealing && (state.shown.load(Ordering::Relaxed) || (!want_charge && charge_t <= 0.0))))
            {
                // (the window already is where the island stands: only the state ends)
                charge_big = false;
                charge_t = 0.0;
                revealing = false;
                reveal_t = 0.0;
            }
            let charge_shown = if charge_big && !revealing { (charge_t / dwell_s.max(0.001)).clamp(0.0, 1.0) } else { 0.0 };

            let pad = pad_full;
            let half_w = win_w / 2.0;
            let clamp_x = |x: f64| -> f64 {
                // same gap from the side edges as the pill keeps from the top (PEEK_MARGIN)
                x.max((geo.x + PEEK_MARGIN) as f64)
                    .min((geo.x + geo.width - PEEK_MARGIN) as f64 - win_w)
            };

            let dragging = state.dragging.load(Ordering::Relaxed);
            // hover = cursor over the pill's own last-drawn rect (see IslandState's doc comment)
            let hovering = (cx as f64) >= pos_x
                && (cx as f64) < pos_x + win_w
                && (cy as f64) >= pos_y
                && (cy as f64) < pos_y + win_h;
            // pinned and left alone for the hide delay: it shrinks (and grows back when touched)
            // (a notification banner or a session pill counts as being looked at: it never shrinks)
            let brief_up = matches!(current_view, PillView::Notification | PillView::Brief);
            if user_pin && !hovering && !dragging && !brief_up {
                pin_idle_ticks += (real_dt / dt).round().max(1.0) as u64;
            } else {
                pin_idle_ticks = 0;
            }
            let shrink_pct = state.pin_shrink.load(Ordering::Relaxed);
            let shrunk = user_pin
                && shrink_pct < 100
                && pin_idle_ticks >= (state.idle_hide_ms.load(Ordering::Relaxed) / EDGE_POLL_MS).max(1);
            let mut shown = state.shown.load(Ordering::Relaxed);
            if shown {
                hide_anim = None; // something brought it back mid-slide
            }
            let edge_y = geo.y + EDGE_TRIGGER_PX;
            let hidden_y = (geo.y - HIDDEN_OFFSET) as f64;
            let peek_y = (geo.y + top_px) as f64;

            if dragging {
                // manual, X-only-from-the-user's-perspective drag -- see
                // drag_start's doc comment. Y isn't left untouched, though:
                // it's pinned to *whichever monitor's* top-edge offset the
                // cursor is currently over, so dragging across a monitor
                // boundary re-anchors it to that monitor's own edge instead
                // of carrying over the Y the drag started at (which could
                // be floating above or below the new monitor's actual top
                // if the two monitors aren't aligned at y=0 together).
                // `geo` above already re-resolves to the cursor's live
                // monitor every tick, so no special-case is needed here.
                let target_x = (cx as f64) - state.drag_grab_dx.load(Ordering::Relaxed) as f64;
                pos_x = target_x
                    .max((geo.x + PEEK_MARGIN) as f64)
                    .min((geo.x + geo.width - PEEK_MARGIN) as f64 - win_w);
                pos_y = (geo.y + top_px) as f64;
                place_window(hwnd, &mut placed, pos_x, pos_y, win_w, win_h, pad);
            } else {
                let cursor_at_edge = cy <= edge_y;

                if !shown {
                    // hidden pills just follow the cursor's monitor silently
                    // (no dash -- nothing visible to dash)
                    pill_monitor_x = geo.x;
                    pinned_dash_target = None;
                    dashed_to_center = false;
                }
                if !shown && cursor_at_edge && (dwell_s <= 0.0 || charge_ready || hide_anim.is_some() || size_anim.is_some()) {
                    shown = true;
                    state.shown.store(true, Ordering::Relaxed);
                    edge_revealed = true;
                    // (mid-slide it is still visible: keep its x, the cursor
                    // follow below glides it over instead of a visible jump)
                    let home_x = if state.show_at_cursor.load(Ordering::Relaxed) {
                        clamp_x(cx as f64 - half_w)
                    } else {
                        // screen center of the monitor the cursor is on
                        clamp_x(geo.x as f64 + geo.width as f64 / 2.0 - half_w)
                    };
                    if hide_anim.take().is_none() {
                        pos_x = home_x;
                    } else {
                        // the hide glide was cut off part-way: it was heading for the center
                        // and would otherwise be left stranded wherever it got to (cursor
                        // follow only helps when it is on, and the cursor is at the edge)
                        vel_dash_x = 0.0;
                        pinned_dash_target = Some(home_x);
                        dashed_to_center = true;
                    }
                    idle_ticks = 0;
                }

                // only the hub and a notification banner park the pill; a
                // media/game/work view peeks for `peek_duration_s` and then
                // slides away like the idle one (edge-hover brings it back)
                let pinned = matches!(
                    current_view,
                    PillView::Hub | PillView::Notification | PillView::UsagePeek | PillView::Brief
                );
                if shown {
                    // any status view (media/game/work/notification/hub/usage)
                    // isn't the edge-hover reveal -- it's a status, so it
                    // dashes over to a *different* monitor when the cursor
                    // goes there, even mid-peek. Triggered once on the monitor
                    // actually changing (not a continuous pull -- that would
                    // fight a manual drag that repositions it within its
                    // current monitor, sliding it back to center right
                    // after), then sprung over several ticks, not teleported.
                    // The eyes-only pill dashes too; every view lands on the
                    // new monitor's center and restarts its hide countdown.
                    // self-heal: also dash when the pill's midpoint is not on the
                    // cursor's monitor at all (a dash that was cut short, or a
                    // crossing that landed while hovering) -- otherwise it sits
                    // stranded until the cursor happens to re-cross
                    let mid = pos_x + win_w / 2.0;
                    let stranded = pinned_dash_target.is_none()
                        && (mid < geo.x as f64 || mid >= (geo.x + geo.width) as f64);
                    if !hovering && !user_pin && (geo.x != pill_monitor_x || stranded) {
                        pill_monitor_x = geo.x;
                        vel_dash_x = 0.0;
                        idle_ticks = 0;
                        dashed_to_center = true;
                        pinned_dash_target =
                            Some(clamp_x(geo.x as f64 + geo.width as f64 / 2.0 - half_w));
                    }
                    if let Some(target) = pinned_dash_target {
                        pos_x = spring_step(pos_x, target, &mut vel_dash_x, dt);
                        // a resize running at the same time must keep growing
                        // from wherever the dash has got to, not snap it back
                        if let Some(anim) = size_anim.as_mut() {
                            anim.2 = pos_x + win_w / 2.0;
                        }
                        if spring_settled(pos_x, target, vel_dash_x) {
                            pos_x = target;
                            pinned_dash_target = None;
                        }
                    }
                    if pinned {
                        idle_ticks = 0;
                    } else if hovering || cursor_at_edge || user_pin {
                        idle_ticks = 0;
                        if cursor_at_edge
                            && !hovering
                            && !dashed_to_center
                            && state.cursor_follow.load(Ordering::Relaxed)
                        {
                            // follow the cursor horizontally while it dwells at the edge
                            let target = clamp_x(cx as f64 - half_w);
                            pos_x = lerp(pos_x, target, NUDGE_EASE);
                        }
                    } else {
                        // counted in nominal ticks, but advanced by the real time that passed
                        idle_ticks += (real_dt / dt).round().max(1.0) as u64;
                        let hide_ms = if edge_revealed || current_view == PillView::Idle {
                            state.idle_hide_ms.load(Ordering::Relaxed)
                        } else {
                            state.peek_ms.load(Ordering::Relaxed)
                        };
                        let idle_hide_ticks = hide_ms / EDGE_POLL_MS;
                        if idle_ticks >= idle_hide_ticks {
                            shown = false;
                            state.shown.store(false, Ordering::Relaxed);
                            // slide up off the pill's own monitor while gliding
                            // back to that monitor's center (the next reveal
                            // starts from a clean, centered position)
                            let hg = winutil::monitor_geometry_at(
                                (pos_x + half_w) as i32,
                                (pos_y + win_h / 2.0) as i32,
                            )
                            .unwrap_or(geo);
                            let to_x = (hg.x as f64 + hg.width as f64 / 2.0 - half_w)
                                .max((hg.x + PEEK_MARGIN) as f64)
                                .min((hg.x + hg.width - PEEK_MARGIN) as f64 - win_w);
                            state
                                .center_x
                                .store(hg.x + hg.width / 2, Ordering::Relaxed);
                            hide_anim = Some((0.0, pos_y, hg.y as f64 - win_h - 8.0, pos_x, to_x));
                            vel_y = 0.0;
                        }
                    }
                }

                if let Some(a) = hide_anim.as_mut() {
                    a.0 += dt;
                    let p = (a.0 / HIDE_SECS).min(1.0);
                    pos_y = a.1 + (a.2 - a.1) * ease_in_back(p);
                    pos_x = a.3 + (a.4 - a.3) * ease_in_out(p);
                    if p >= 1.0 {
                        hide_anim = None; // fully off-screen now: park it
                    }
                } else {
                    let target_y = if charge_big || shown {
                        peek_y
                    } else {
                        hidden_y
                    };
                    pos_y = spring_step(pos_y, target_y, &mut vel_y, dt);
                }

                place_window(hwnd, &mut placed, pos_x, pos_y, win_w, win_h, pad);

                // another topmost window (or the taskbar, a popup) can end up above the island and
                // nothing here would notice: while it is up, claim the top again as it appears and
                // about twice a second after
                let up = charge_big || shown;
                if up && (!was_up || last_raise.elapsed() >= Duration::from_millis(500)) {
                    winutil::raise(hwnd);
                    last_raise = Instant::now();
                }
                was_up = up;
            }

            // only the island itself takes the mouse: the margin around it must let clicks
            // through to whatever lies beneath (and the whole window does while the edge
            // glow charges)
            {
                let over = (cx as f64) >= pos_x - 1.0
                    && (cx as f64) < pos_x + win_w + 1.0
                    && (cy as f64) >= pos_y - 1.0
                    && (cy as f64) < pos_y + win_h + 1.0;
                let want_ignore = (charge_big && !revealing) || !(over || state.dragging.load(Ordering::Relaxed));
                // (checked every tick, cheaply: it also re-asserts the style if anything reset it)
                ignoring = want_ignore;
                winutil::click_through(hwnd, ignoring);
            }

            {
                let shown = state.shown.load(Ordering::Relaxed);
                let local = ((cx as f64 - (pos_x - pad)) / scale, (cy as f64 - (pos_y - pad)) / scale);
                let _ = window.emit(
                    "cursor-tick",
                    CursorTick {
                        local_x: local.0,
                        local_y: local.1,
                        shown,
                        shrunk,
                        shrink: shrink_pct,
                        charge: charge_shown,
                        big: charge_big,
                        pad: pad / scale,
                        edge: (geo.y as f64 - (pos_y - pad)) / scale,
                        pill_top: (geo.y as f64 - pos_y + top_px as f64) / scale,
                        pill_w: view_size(current_view, wpct, hub_w).0,
                        pill_h: if current_view == PillView::Hub {
                            state.hub_height.load(Ordering::Relaxed) as f64
                        } else {
                            current_view.size().1
                        },
                    },
                );
            }
        }
    });
}

pub fn run() {
    tauri::Builder::default()
        .manage(Arc::new(IslandState::default()))
        .invoke_handler(tauri::generate_handler![
            browse::click_page_button,
            downloads::download_item_click,
            drag_start,
            push_notification,
            get_notification_history,
            toggle_hub,
            close_hub,
            set_hub_height,
            open_notification_action,
            dismiss_notification,
            click_notification,
            focus::focus_source,
            toggle_pin,
            media::media_play_pause,
            media::media_next,
            media::media_previous,
            media::media_seek,
            media::media_seek_by,
            media::media_set_rate,
            settings::get_settings,
            calendar::get_calendar,
            activity::get_activity,
            gpu::get_gpu_pct,
            gamestats::get_game_stats,
            llm::llm_dismiss,
            clock::time_preview,
            usage::get_usage,
            usage::refresh_usage,
            stats::get_sys_stats,
            settings::save_settings,
            background::pick_background,
            background::clear_background,
        ])
        .setup(|app| {
            let window = app.get_webview_window("island").expect("island window must exist");

            // the island is not a browser page: no refresh, print, find, devtools, zoom, context menu...
            lock_down_webview(&window);

            // park off-screen above the primary monitor's top edge until the
            // edge-poll loop's first tick decides whether to reveal it
            if let Some(monitor) = window.primary_monitor()? {
                let geo = monitor.position();
                let size = window.outer_size()?;
                let center_x = geo.x + monitor.size().width as i32 / 2 - size.width as i32 / 2;
                window.set_position(PhysicalPosition::new(center_x, geo.y - HIDDEN_OFFSET))?;
                window
                    .state::<Arc<IslandState>>()
                    .center_x
                    .store(geo.x + monitor.size().width as i32 / 2, Ordering::Relaxed);
            }
            window.show()?;

            // clicking anywhere outside the island (losing focus) closes the
            // hub, same as the Qt version's click-outside behavior. Skipped
            // while a click/drag gesture is in progress -- starting
            // `start_dragging()`'s native move-loop itself causes a
            // transient focus-loss/regain that would otherwise fire this
            // and force-close a hub the same click just opened.
            {
                let hub_state = window.state::<Arc<IslandState>>().inner().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        // pinned: click-outside no longer collapses the hub either
                        // (also while the background file dialog is open -- it takes focus)
                        if !hub_state.dragging.load(Ordering::Relaxed)
                            && !hub_state.pinned.load(Ordering::Relaxed)
                            && !background::PICKING.load(Ordering::Relaxed)
                        {
                            hub_state.hub_open.store(false, Ordering::Relaxed);
                        }
                    }
                });
            }

            spawn_edge_poll(window.clone());
            media::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            game::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            work::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            usage::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            calendar::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            audio::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            project::spawn(window.state::<Arc<IslandState>>().inner().clone());
    browse::spawn(window.state::<Arc<IslandState>>().inner().clone());
    downloads::spawn(window.state::<Arc<IslandState>>().inner().clone());
    // screenshot aid: `DI_OPEN_SETTINGS=1` opens the hub on its settings pane shortly after launch
    if std::env::var_os("DI_OPEN_SETTINGS").is_some() {
        let w = window.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            let state = w.state::<Arc<IslandState>>();
            let _ = w.emit("open-settings", ());
            state.hub_open.store(true, Ordering::Relaxed);
            state.shown.store(true, Ordering::Relaxed);
        });
    }
            llm::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            clock::spawn(window.state::<Arc<IslandState>>().inner().clone());
            notify_listener::spawn(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());
            notify_listener::spawn_viber(app.handle().clone(), window.state::<Arc<IslandState>>().inner().clone());

            #[cfg(debug_assertions)]
            tauri::Listener::listen(&window, "js-error", |e| eprintln!("js-error: {}", e.payload()));

            // debug-only: `DI_DEMO_NOTIFY=1` pushes a banner shortly after launch
            // and logs the window's size around it, for eyeballing the resize
            #[cfg(debug_assertions)]
            if std::env::var_os("DI_DEMO_NOTIFY").is_some() {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(5));
                    let state = w.state::<Arc<IslandState>>();
                    eprintln!("before: {:?}", w.outer_size());
                    state.notif.lock().unwrap().push("Demo".into(), "Hello from the demo banner".into());
                    for i in 0..8 {
                        std::thread::sleep(Duration::from_millis(500));
                        eprintln!("t+{}ms: {:?} pos {:?}", (i + 1) * 500, w.outer_size(), w.outer_position());
                    }
                });
            }

            // `DI_DEMO_BRIEF=1`: a session that needs you, then one that finished, 5 s after launch
            if std::env::var_os("DI_DEMO_BRIEF").is_some() {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(5));
                    let state = w.state::<Arc<IslandState>>();
                    if std::env::var_os("DI_DEMO_TIME").is_some() {
                        clock::push(&state, false);
                        return;
                    }
                    let b = |id: &str, state: &'static str, ctx: f64| notify::Brief { id: id.into(), state, host_icon: "code", host_exe: None, project: "demo".into(), ctx };
                    let mut n = state.notif.lock().unwrap();
                    n.push_brief("Brief-show island queue".into(), b("a", "finished", 0.26));
                    n.push_brief("Settings redesign needs a decision".into(), b("b", "waiting", 0.7));
                });
            }

            // debug-only: `DI_DEMO_EMOTE=xd` pins an eye expression 6 s after launch
            #[cfg(debug_assertions)]
            if let Some(name) = std::env::var_os("DI_DEMO_EMOTE") {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(6));
                    let _ = w.eval(&format!("window.__setEmote('{}')", name.to_string_lossy()));
                });
            }

            // debug-only: `DI_DEMO_PIN=1` right-clicks the pill (via a synthetic DOM event) 6 s after launch
            #[cfg(debug_assertions)]
            if std::env::var_os("DI_DEMO_PIN").is_some() {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(6));
                    let _ = w.eval("document.getElementById('pill').dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true,clientX:40,clientY:12}))");
                });
            }

            // debug-only: `DI_DEMO_IDLE=1` forces the plain eyes pill (hub closed) and parks the cursor at the top edge
            #[cfg(debug_assertions)]
            if std::env::var_os("DI_DEMO_IDLE").is_some() {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(4));
                    let state = w.state::<Arc<IslandState>>();
                    state.hub_open.store(false, Ordering::Relaxed);
                    state.shown.store(true, Ordering::Relaxed);
                });
            }

            // debug-only: `DI_DEMO_HUB=1` opens the hub shortly after launch
            #[cfg(debug_assertions)]
            if std::env::var_os("DI_DEMO_HUB").is_some() {
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(20));
                    let state = w.state::<Arc<IslandState>>();
                    if std::env::var("DI_DEMO_HUB").as_deref() == Ok("settings") {
                        let _ = w.emit("open-settings", ());
                    }
                    state.hub_open.store(true, Ordering::Relaxed);
                    state.shown.store(true, Ordering::Relaxed);
                    if std::env::var_os("DI_DEMO_EYES").is_some() {
                        for mood in ["happy", "sleepy", "wide", "none"] {
                            let _ = w.eval(&format!("__mood.force = {}", if mood == "none" { "null".to_string() } else { format!("'{mood}'") }));
                            std::thread::sleep(Duration::from_millis(1500));
                        }
                    }
                    if std::env::var_os("DI_DEMO_TIP").is_some() {
                        tauri::Listener::listen(&w, "dbg", |e| eprintln!("tipdebug: {}", e.payload()));
                        std::thread::sleep(Duration::from_secs(3));
                        let _ = w.eval(
                            "try{const r=document.getElementById('ring-ram');r.dispatchEvent(new MouseEvent('mouseover',{bubbles:true}));setTimeout(()=>{const t=document.getElementById('tip');window.__TAURI__.event.emit('dbg',t.className+'|'+t.textContent+'|'+t.style.left+','+t.style.top)},500)}catch(e){window.__TAURI__.event.emit('dbg','ERR '+e)}",
                        );
                        std::thread::sleep(Duration::from_secs(2));
                    }
                });
            }

            let open_hub = tauri::menu::MenuItemBuilder::with_id("open_hub", "Open Hub").build(app)?;
            let settings_item = tauri::menu::MenuItemBuilder::with_id("settings", "Settings").build(app)?;
            let show = tauri::menu::MenuItemBuilder::with_id("show", "Show Island").build(app)?;
            let quit = tauri::menu::MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = tauri::menu::MenuBuilder::new(app)
                .items(&[&open_hub, &settings_item, &show])
                .separator()
                .items(&[&quit])
                .build()?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().unwrap())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("NADI")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "open_hub" => open_hub_from_tray(app),
                    "settings" => {
                        // emit before opening so the frontend knows which pane
                        // to show by the time the hub view arrives
                        let _ = app.emit("open-settings", ());
                        open_hub_from_tray(app);
                    }
                    "show" => {
                        if let Some(w) = app.get_webview_window("island") {
                            w.state::<Arc<IslandState>>().shown.store(true, Ordering::Relaxed);
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // left click = open hub, right click = menu
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        open_hub_from_tray(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running NADI");
}
