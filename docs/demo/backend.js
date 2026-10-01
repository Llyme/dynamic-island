// The Rust side of NADI (src-tauri/src/lib.rs, notify.rs, settings.rs, clock.rs), ported to JavaScript so the
// island's real frontend (demo/ui) can run in a web page. The window-placement state machine, the springs, the
// edge dwell and glow charge, the hide/peek timers, the notification queue and the settings are the same code
// paths, line for line; what differs is only where the numbers come from: the "screen" is the mock desktop, the
// cursor is the pointer over it, and the PC the island watches is the simulation in world.js.
(() => {
  "use strict";

  const now = () => performance.now() / 1000;
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

  // ---------------------------------------------------------------- constants (lib.rs)
  const EDGE_TRIGGER_PX = 3;
  const HIDDEN_OFFSET = 200;
  const PEEK_MARGIN = 4;
  const HIDE_SECS = 0.45;
  const NUDGE_EASE = 0.16;
  const GLOW_PAD = 44;
  const REVEAL_SECS = 0.6;
  const EDGE_POLL_MS = 16;
  const COMPACT_BASE = 260;
  const SIZES = {
    idle: [140, 28],
    media: [260, 40],
    game: [320, 40],
    work: [260, 40],
    notification: [140, 72],
    usage_peek: [240, 78],
    brief: [140, 40],
    hub: [420, 520],
  };
  const HUB_MIN_H = 150;
  const HUB_MAX_H = 520;
  const USAGE_PEEK_TOTAL_MS = 500 + 1400 + 2500;
  const SPRING_STIFFNESS = 1300;
  const SPRING_DAMPING = 48;
  const SPRING_SETTLE_DIST = 0.4;
  const SPRING_SETTLE_VEL = 2;

  function viewSize(view, width, hubW) {
    const [w, h] = SIZES[view];
    const k = width / COMPACT_BASE;
    if (view === "hub") return [hubW, h];
    if (view === "notification" || view === "brief") return [width, h];
    return [w * k, h];
  }

  const lerp = (c, t, e) => c + (t - c) * e;
  const springStep = (value, target, vel, dt) => {
    vel.v += (-SPRING_STIFFNESS * (value - target) - SPRING_DAMPING * vel.v) * dt;
    return value + vel.v * dt;
  };
  const easeInBack = (p) => {
    const c1 = 1.5;
    return (c1 + 1) * p * p * p - c1 * p * p;
  };
  const easeInOut = (p) => p * p * (3 - 2 * p);
  const springSettled = (value, target, vel) => Math.abs(value - target) < SPRING_SETTLE_DIST && Math.abs(vel) < SPRING_SETTLE_VEL;

  // ---------------------------------------------------------------- settings (settings.rs)
  const DEFAULTS = {
    game_detection: true,
    work_detection: true,
    download_detection: true,
    llm_detection: true,
    llm_brief: true,
    start_with_windows: false,
    idle_hide_delay_s: 1,
    peek_duration_s: 3,
    edge_dwell_ms: 300,
    pin_shrink: 75,
    compact_width: 260,
    hub_width: 420,
    top_margin: 4,
    show_at_cursor: true,
    cursor_follow: true,
    react_to_audio: true,
    show_eyes: true,
    accent_color: "#5ac88c",
    time_announce: false,
    time_interval: 4,
    time_24h: false,
    audio_bleed: 60,
    calendar_ics_url: "https://calendar.google.com/calendar/ical/you%40example.com/private-demo/basic.ics",
    calendar_reminder_lead_min: 15,
    calendar_poll_min: 10,
    bg_compact: "",
    bg_hub: "",
    bg_dim: 50,
    glow_intensity: 70,
    page_preview: true,
  };
  const SETTINGS_KEY = "nadi-demo-settings-v1";
  let settings = (() => {
    try {
      return { ...DEFAULTS, ...JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}"), bg_compact: "", bg_hub: "" };
    } catch (_) {
      return { ...DEFAULTS };
    }
  })();
  const persist = () => {
    try {
      localStorage.setItem(SETTINGS_KEY, JSON.stringify({ ...settings, bg_compact: "", bg_hub: "" }));
    } catch (_) {}
  };
  const compactPx = () => {
    const v = settings.compact_width;
    return clamp(v <= 160 ? (v * 260) / 100 : v, 180, 560);
  };

  // ---------------------------------------------------------------- notification queue (notify.rs)
  const NOTIF_MS = 4000;
  const HISTORY_CAP = 50;
  const GROUP_AFTER = 3;

  class NotifState {
    constructor() {
      this.current = null;
      this.deadline = null;
      this.queue = [];
      this.hist = [];
      this.nextId = 0;
    }
    dwell(n) {
      const s = n.brief && n.brief.state;
      return (s === "waiting" ? 8000 : s === "time" ? 4000 : s ? 5000 : NOTIF_MS) / 1000;
    }
    push(title, body) {
      this.pushAction(title, body, null);
    }
    pushAction(title, body, action) {
      this.nextId += 1;
      const same = (e) => e.title === title && e.body === body && e.action === action;
      this.hist = this.hist.filter((e) => !same(e));
      this.queue = this.queue.filter((n) => !same(n));
      let count = 1;
      if (action === "discord") {
        const before = this.hist.filter((e) => e.action === action).reduce((s, e) => s + e.count, 0);
        if (before + 1 > GROUP_AFTER) {
          count = before + 1;
          this.hist = this.hist.filter((e) => e.action !== action);
          this.queue = this.queue.filter((n) => n.action !== action);
          title = "Discord";
          body = `${count} new messages`;
        }
      }
      this.hist.unshift({ id: this.nextId, title, body, time_ms: Date.now(), action, count });
      this.hist.length = Math.min(this.hist.length, HISTORY_CAP);
      this.queue.push({ id: this.nextId, title, body, action, brief: null });
    }
    pushBrief(title, brief) {
      this.queue = this.queue.filter((n) => !(n.brief && n.brief.id === brief.id));
      const n = { id: 0, title, body: "", action: null, brief };
      if (brief.state === "waiting") {
        let at = this.queue.findIndex((q) => !(q.brief && q.brief.state === "waiting"));
        if (at < 0) at = this.queue.length;
        this.queue.splice(at, 0, n);
      } else {
        this.queue.push(n);
      }
    }
    cancelBrief(id) {
      this.queue = this.queue.filter((n) => !(n.brief && n.brief.id === id));
      if (this.current && this.current.brief && this.current.brief.id === id && this.current.brief.state === "waiting") this.dismissCurrent();
    }
    currentIsBrief() {
      return !!(this.current && this.current.brief);
    }
    dismissCurrent() {
      if (this.current) this.deadline = now();
    }
    history() {
      return this.hist.map((e) => ({ ...e }));
    }
    remove(id) {
      this.hist = this.hist.filter((e) => e.id !== id);
    }
    hasCurrent() {
      return !!this.current;
    }
    // advances the queue; returns undefined when nothing changed, else { n } (n null = the banner ended)
    tick() {
      const t = now();
      const expired = this.deadline !== null && t >= this.deadline;
      if (!this.current || expired) {
        const next = this.queue.shift() || null;
        const changed = !!next || !!this.current;
        this.current = next;
        this.deadline = next ? t + this.dwell(next) : null;
        if (changed) return { n: next };
      }
      return undefined;
    }
  }

  // ---------------------------------------------------------------- the NADI object
  const NADI = (window.NADI = {
    now,
    clamp,
    settings: () => settings,
    persist: () => persist(),
    notif: new NotifState(),
    cmds: {},
    frame: null,
    desk: null,
    // island state (IslandState in lib.rs)
    st: {
      dragging: false,
      shown: false,
      hubOpen: false,
      hubHeight: HUB_MAX_H,
      dragStart: null,
      dragGrabDx: 0,
      centerX: 0,
      pinned: false,
      usagePeekUntil: 0,
      peekRequest: false,
      hasMedia: false,
      hasGame: false,
      hasWork: false,
      posX: 0,
      posY: 0,
      winW: 140,
      winH: 28,
      picking: false,
    },
    ptr: { x: 0, y: 1e6, left: false, inside: false },
  });
  const st = NADI.st;
  const ptr = NADI.ptr;

  NADI.emit = (name, payload) => {
    try {
      NADI.frame.contentWindow.__nadiEmit(name, payload);
    } catch (_) {}
  };
  NADI.pushHistory = () => NADI.emit("notification-history-tick", NADI.notif.history());

  NADI.invoke = async (cmd, args) => {
    const fn = NADI.cmds[cmd];
    if (!fn) {
      console.warn("demo: command not simulated:", cmd);
      return null;
    }
    return fn(args || {});
  };

  // ---------------------------------------------------------------- pointer: the cursor and its button
  const BEZEL = 40; // the strip above the desktop counts as the screen's edge (the real cursor stops there)
  function hostPointer(clientX, clientY) {
    const r = NADI.desk.getBoundingClientRect();
    const x = clientX - r.left;
    const y = clientY - r.top;
    if (x >= 0 && x < r.width && y >= -BEZEL && y < r.height) {
      ptr.x = x;
      ptr.y = Math.max(0, y);
      ptr.inside = true;
    } else {
      ptr.x = r.width / 2;
      ptr.y = 1e6;
      ptr.inside = false;
    }
  }
  NADI.framePointer = (type, e) => {
    const r = NADI.frame.getBoundingClientRect();
    hostPointer(r.left + e.clientX, r.top + e.clientY);
    if (type === "pointerdown" && e.button === 0) ptr.left = true;
    if (type === "pointerup" || type === "pointercancel") ptr.left = false;
  };
  // the island's window lost focus: the hub closes unless it is pinned
  NADI.blur = () => {
    if (!st.dragging && !st.pinned && !st.picking) st.hubOpen = false;
  };
  NADI.frameBlur = () => {};

  // ---------------------------------------------------------------- commands (lib.rs)
  const C = NADI.cmds;
  C.toggle_hub = () => {
    st.hubOpen = !st.hubOpen;
    if (st.hubOpen) st.shown = true;
  };
  NADI.openHubFromTray = () => {
    st.hubOpen = true;
    st.shown = true;
  };
  C.toggle_pin = () => {
    st.pinned = !st.pinned;
    if (st.pinned) st.shown = true;
    NADI.emit("pin-tick", st.pinned);
  };
  C.set_hub_height = ({ h }) => {
    st.hubHeight = Math.round(clamp(h, HUB_MIN_H, HUB_MAX_H));
  };
  C.close_hub = () => {
    st.hubOpen = false;
  };
  C.push_notification = ({ title, body }) => {
    NADI.notif.push(title, body);
    NADI.pushHistory();
  };
  C.open_notification_action = ({ action }) => {
    if (action === "viber" || action === "discord") NADI.focusApp?.("chat", true);
    NADI.notif.dismissCurrent();
  };
  C.click_notification = ({ id, action }) => {
    if (action) C.open_notification_action({ action });
    NADI.notif.remove(id);
    NADI.notif.dismissCurrent();
    NADI.pushHistory();
  };
  C.dismiss_notification = ({ id }) => {
    NADI.notif.remove(id);
    NADI.pushHistory();
  };
  C.get_notification_history = () => NADI.notif.history();
  C.drag_start = () => {
    st.dragging = true;
    st.dragStart = [st.posX, st.posY];
    st.dragGrabDx = ptr.x - st.posX;
  };
  C.get_settings = () => ({ ...settings });
  C.save_settings = ({ settings: s }) => {
    const accent = settings.accent_color;
    settings = { ...s };
    persist();
    if (settings.accent_color !== accent) NADI.onAccent?.(settings.accent_color);
  };

  // ---------------------------------------------------------------- the poll loop (spawn_edge_poll)
  function startLoop() {
    const desk = NADI.desk;
    const frame = NADI.frame;
    const geo = { x: 0, y: 0, width: desk.clientWidth, height: desk.clientHeight };
    new ResizeObserver(() => {
      geo.width = desk.clientWidth;
      geo.height = desk.clientHeight;
    }).observe(desk);

    const pad = GLOW_PAD;
    let appliedWidth = compactPx();
    let winW = (SIZES.idle[0] * appliedWidth) / COMPACT_BASE;
    let winH = SIZES.idle[1];
    st.centerX = geo.width / 2;
    let posX = st.centerX - winW / 2;
    let posY = geo.y - HIDDEN_OFFSET + pad;
    let placed = [NaN, 0, 0, 0];
    let pinIdleTicks = 0;
    let idleTicks = 0;
    let currentView = "idle";
    let pillMonitorX = -Infinity;
    let wasPinned = false;
    let pinnedDashTarget = null;
    let dashedToCenter = false;
    const velY = { v: 0 };
    const velDashX = { v: 0 };
    const velW = { v: 0 };
    const velH = { v: 0 };
    let sizeAnim = null; // [targetW, targetH, anchorMidX]
    let hideAnim = null; // { t, fromY, toY, fromX, toX }
    let appliedHubH = HUB_MAX_H;
    let appliedHubW = settings.hub_width;
    let chargeT = 0;
    let chargeBig = false;
    let revealing = false;
    let edgeRevealed = false;
    let revealT = 0;
    const dt = EDGE_POLL_MS / 1000;
    let ignoring = null;

    function place(x, y, w, h) {
      const rect = [Math.round(x - pad), Math.round(y - pad), Math.round(w + 2 * pad), Math.round(h + 2 * pad)];
      if (rect[0] !== placed[0] || rect[1] !== placed[1] || rect[2] !== placed[2] || rect[3] !== placed[3]) {
        placed = rect;
        frame.style.transform = `translate(${rect[0]}px, ${rect[1]}px)`;
        if (frame._w !== rect[2]) frame.style.width = `${(frame._w = rect[2])}px`;
        if (frame._h !== rect[3]) frame.style.height = `${(frame._h = rect[3])}px`;
      }
    }

    function tick() {
      const realDt = dt;
      const wpct = compactPx();
      const hubW = clamp(settings.hub_width, 340, 640);
      const topPx = clamp(settings.top_margin, 0, 80);
      const cx = ptr.x;
      const cy = ptr.y;
      const userPin = st.pinned;
      wasPinned = userPin;

      // end of a drag: a click that went nowhere toggles the hub
      if (st.dragging && !ptr.left) {
        st.dragging = false;
        st.centerX = posX + winW / 2;
        pillMonitorX = geo.x;
        pinnedDashTarget = null;
        const start = st.dragStart;
        st.dragStart = null;
        const moved = start ? Math.abs(start[0] - posX) > 3 || Math.abs(start[1] - posY) > 3 : false;
        if (!moved) {
          st.hubOpen = !st.hubOpen;
          if (st.hubOpen) st.shown = true;
        }
      }

      // notifications: their own dwell timer, outranking every view but the hub
      const adv = NADI.notif.tick();
      if (adv) NADI.emit("notification-tick", adv.n);
      const hasNotification = NADI.notif.hasCurrent();
      const isBrief = NADI.notif.currentIsBrief();

      const wantView = st.hubOpen
        ? "hub"
        : hasNotification
          ? isBrief
            ? "brief"
            : "notification"
          : st.usagePeekUntil > now()
            ? "usage_peek"
            : st.hasMedia
              ? "media"
              : st.hasGame
                ? "game"
                : st.hasWork
                  ? "work"
                  : "idle";
      if (wantView !== currentView) {
        currentView = wantView;
        let [lw, lh] = viewSize(wantView, wpct, hubW);
        if (wantView === "hub") {
          lh = st.hubHeight;
          appliedHubH = lh;
        }
        sizeAnim = [lw, lh, posX + winW / 2];
        velW.v = 0;
        velH.v = 0;
        NADI.emit("view-tick", wantView);
        if (wantView !== "idle") {
          st.shown = true;
          edgeRevealed = false;
          idleTicks = 0;
        }
      }

      // the width setting changed
      if (wpct !== appliedWidth) {
        appliedWidth = wpct;
        if (currentView !== "hub" && !chargeBig) {
          const [lw, lh] = viewSize(currentView, wpct, hubW);
          sizeAnim = [lw, lh, posX + winW / 2];
          velW.v = 0;
          velH.v = 0;
        }
      }

      // hub open and its content grew/shrank
      if (currentView === "hub") {
        const wantH = st.hubHeight;
        if (Math.abs(wantH - appliedHubH) >= 1 || Math.abs(hubW - appliedHubW) >= 1) {
          appliedHubH = wantH;
          appliedHubW = hubW;
          const anchor = sizeAnim ? sizeAnim[2] : posX + winW / 2;
          sizeAnim = [hubW, wantH, anchor];
        }
      }

      if (sizeAnim) {
        const [tw, th, anchor] = sizeAnim;
        winW = springStep(winW, tw, velW, dt);
        winH = springStep(winH, th, velH, dt);
        if (springSettled(winW, tw, velW.v) && springSettled(winH, th, velH.v)) {
          winW = tw;
          winH = th;
          sizeAnim = null;
        }
        posX = anchor - winW / 2;
        if (pinnedDashTarget === null) {
          posX = Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, posX));
        }
      }

      if (st.peekRequest) {
        st.peekRequest = false;
        if (currentView === "media" || currentView === "game" || currentView === "work") {
          st.shown = true;
          edgeRevealed = false;
          idleTicks = 0;
        }
      }

      // edge dwell: resting at the top edge calls the island, a glow builds meanwhile
      const dwellS = clamp(settings.edge_dwell_ms, 0, 2000) / 1000;
      const wantCharge = dwellS > 0 && !st.shown && !st.dragging && cy <= geo.y + EDGE_TRIGGER_PX && hideAnim === null && sizeAnim === null;
      let chargeReady = false;
      if (wantCharge) {
        if (!chargeBig) {
          const [lw, lh] = viewSize(currentView, wpct, hubW);
          const mid = settings.show_at_cursor ? cx : geo.x + geo.width / 2;
          winW = lw;
          winH = lh;
          posX = Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, mid - winW / 2));
          posY = geo.y + topPx;
          velY.v = 0;
          chargeBig = true;
        }
        chargeT += realDt;
        if (settings.show_at_cursor) {
          const want = Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, cx - winW / 2));
          posX = lerp(posX, want, 0.25);
        }
        chargeReady = chargeT >= dwellS;
      } else if (chargeT > 0) {
        chargeT = Math.max(0, chargeT - realDt * 4 * Math.max(dwellS, 0.05));
      }
      if (chargeReady && chargeBig && !revealing) {
        revealing = true;
        revealT = 0;
        chargeT = 0;
      }
      if (revealing) revealT += realDt;
      const revealDone = revealing && revealT >= REVEAL_SECS;
      if (chargeBig && (revealDone || (!revealing && (st.shown || (!wantCharge && chargeT <= 0))))) {
        chargeBig = false;
        chargeT = 0;
        revealing = false;
        revealT = 0;
      }
      const chargeShown = chargeBig && !revealing ? clamp(chargeT / Math.max(dwellS, 0.001), 0, 1) : 0;

      const halfW = winW / 2;
      const clampX = (x) => Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, x));
      const dragging = st.dragging;
      const hovering = cx >= posX && cx < posX + winW && cy >= posY && cy < posY + winH;
      // a notification banner, a session pill or the usage peek counts as being looked at: it never shrinks
      const briefUp = currentView === "notification" || currentView === "brief" || currentView === "usage_peek";
      if (userPin && !hovering && !dragging && !briefUp) pinIdleTicks += Math.max(1, Math.round(realDt / dt));
      else pinIdleTicks = 0;
      const shrinkPct = clamp(settings.pin_shrink, 30, 100);
      const idleHideMs = Math.max(1, settings.idle_hide_delay_s) * 1000;
      const shrunk = userPin && !briefUp && shrinkPct < 100 && pinIdleTicks >= Math.max(1, Math.floor(idleHideMs / EDGE_POLL_MS));
      let shown = st.shown;
      if (shown) hideAnim = null;
      const edgeY = geo.y + EDGE_TRIGGER_PX;
      const hiddenY = geo.y - HIDDEN_OFFSET;
      const peekY = geo.y + topPx;

      if (dragging) {
        posX = Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, cx - st.dragGrabDx));
        posY = geo.y + topPx;
        place(posX, posY, winW, winH);
      } else {
        const cursorAtEdge = cy <= edgeY;
        if (!shown) {
          pillMonitorX = geo.x;
          pinnedDashTarget = null;
          dashedToCenter = false;
        }
        if (!shown && cursorAtEdge && (dwellS <= 0 || chargeReady || hideAnim !== null || sizeAnim !== null)) {
          shown = true;
          st.shown = true;
          edgeRevealed = true;
          const homeX = settings.show_at_cursor ? clampX(cx - halfW) : clampX(geo.x + geo.width / 2 - halfW);
          if (hideAnim === null) {
            posX = homeX;
          } else {
            hideAnim = null;
            velDashX.v = 0;
            pinnedDashTarget = homeX;
            dashedToCenter = true;
          }
          idleTicks = 0;
        }

        const pinned = currentView === "hub" || currentView === "notification" || currentView === "usage_peek" || currentView === "brief";
        if (shown) {
          const mid = posX + winW / 2;
          const stranded = pinnedDashTarget === null && (mid < geo.x || mid >= geo.x + geo.width);
          if (!hovering && !userPin && (geo.x !== pillMonitorX || stranded)) {
            pillMonitorX = geo.x;
            velDashX.v = 0;
            idleTicks = 0;
            dashedToCenter = true;
            pinnedDashTarget = clampX(geo.x + geo.width / 2 - halfW);
          }
          if (pinnedDashTarget !== null) {
            posX = springStep(posX, pinnedDashTarget, velDashX, dt);
            if (sizeAnim) sizeAnim[2] = posX + winW / 2;
            if (springSettled(posX, pinnedDashTarget, velDashX.v)) {
              posX = pinnedDashTarget;
              pinnedDashTarget = null;
            }
          }
          if (pinned) {
            idleTicks = 0;
          } else if (hovering || cursorAtEdge || userPin) {
            idleTicks = 0;
            if (cursorAtEdge && !hovering && !dashedToCenter && settings.cursor_follow) {
              posX = lerp(posX, clampX(cx - halfW), NUDGE_EASE);
            }
          } else {
            idleTicks += Math.max(1, Math.round(realDt / dt));
            const hideMs = edgeRevealed || currentView === "idle" ? idleHideMs : Math.max(1, settings.peek_duration_s) * 1000;
            if (idleTicks >= Math.floor(hideMs / EDGE_POLL_MS)) {
              shown = false;
              st.shown = false;
              const toX = Math.max(geo.x + PEEK_MARGIN, Math.min(geo.x + geo.width - PEEK_MARGIN - winW, geo.x + geo.width / 2 - halfW));
              st.centerX = geo.x + geo.width / 2;
              hideAnim = { t: 0, fromY: posY, toY: geo.y - winH - 8, fromX: posX, toX };
              velY.v = 0;
            }
          }
        }

        if (hideAnim) {
          hideAnim.t += dt;
          const p = Math.min(hideAnim.t / HIDE_SECS, 1);
          posY = hideAnim.fromY + (hideAnim.toY - hideAnim.fromY) * easeInBack(p);
          posX = hideAnim.fromX + (hideAnim.toX - hideAnim.fromX) * easeInOut(p);
          if (p >= 1) hideAnim = null;
        } else {
          posY = springStep(posY, chargeBig || shown ? peekY : hiddenY, velY, dt);
        }
        place(posX, posY, winW, winH);
      }

      // only the island itself takes the pointer: the margin around it lets clicks through
      {
        const over = cx >= posX - 1 && cx < posX + winW + 1 && cy >= posY - 1 && cy < posY + winH + 1;
        const wantIgnore = (chargeBig && !revealing) || !(over || st.dragging);
        if (wantIgnore !== ignoring) {
          ignoring = wantIgnore;
          frame.style.pointerEvents = wantIgnore ? "none" : "auto";
        }
      }

      st.posX = posX;
      st.posY = posY;
      st.winW = winW;
      st.winH = winH;
      const sizeNow = viewSize(currentView, wpct, hubW);
      NADI.emit("cursor-tick", {
        local_x: cx - (posX - pad),
        local_y: cy - (posY - pad),
        shown: st.shown,
        shrunk,
        shrink: shrinkPct,
        charge: chargeShown,
        big: chargeBig,
        pad,
        edge: geo.y - (posY - pad),
        pill_top: geo.y - posY + topPx,
        pill_w: sizeNow[0],
        pill_h: currentView === "hub" ? st.hubHeight : SIZES[currentView][1],
      });
    }

    // fixed 16 ms steps (the springs are tuned to that), whatever the display's refresh rate
    let last = performance.now();
    let acc = 0;
    function frameLoop(t) {
      acc += Math.min(0.1, (t - last) / 1000);
      last = t;
      let n = 0;
      while (acc >= dt && n < 6) {
        tick();
        acc -= dt;
        n++;
      }
      requestAnimationFrame(frameLoop);
    }
    requestAnimationFrame(frameLoop);
  }

  // ---------------------------------------------------------------- start (once the island's page has loaded)
  NADI.start = (frame, desk) => {
    NADI.frame = frame;
    NADI.desk = desk;
    for (const type of ["pointermove", "pointerdown", "pointerup", "pointercancel"]) {
      document.addEventListener(
        type,
        (e) => {
          hostPointer(e.clientX, e.clientY);
          if (type === "pointerdown" && e.button === 0) {
            ptr.left = true;
            NADI.blur(); // anything that reaches this page is outside the island
          }
          if (type === "pointerup" || type === "pointercancel") ptr.left = false;
        },
        true,
      );
    }
    startLoop();
    NADI.onStart?.();
  };
})();
