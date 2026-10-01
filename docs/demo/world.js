// The PC that the demo island watches. In the app, Rust reads this from Windows (the media session, the
// foreground window, running games, browser downloads, Claude Code's session files, Windows notifications);
// here it is a small simulation behind the mock desktop, answering with the same payloads and following the
// same rules (work.rs, activity.rs, llm.rs, downloads.rs, calendar.rs, usage.rs, clock.rs). Only the delays
// that would make a demo feel dead are shorter (a window has to be in front for 3 s, not 8).
(() => {
  "use strict";
  const { now, clamp, st, notif, cmds: C } = NADI;
  const S = () => NADI.settings();
  const W = (NADI.world = {
    fg: null, // id of the window in front (null = the desktop)
    win: {}, // id -> { open, min } (kept by desktop.js)
  });

  // ---------------------------------------------------------------- icons and cover art
  const svg = (inner, bg) =>
    "data:image/svg+xml;utf8," +
    encodeURIComponent(`<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><rect width='32' height='32' rx='8' fill='${bg}'/>${inner}</svg>`);
  const stroke = "fill='none' stroke='#fff' stroke-width='2.3' stroke-linecap='round' stroke-linejoin='round'";
  const ICONS = (W.ICONS = {
    code: svg(`<path d='M12 11 7 16l5 5M20 11l5 5-5 5' ${stroke}/>`, "#2f7bd9"),
    term: svg(`<path d='M9 10l6 6-6 6M17 22h7' ${stroke}/>`, "#2a2a31"),
    browser: svg(`<circle cx='16' cy='16' r='8' ${stroke}/><path d='M8 16h16M16 8c3.5 3 3.5 13 0 16M16 8c-3.5 3-3.5 13 0 16' ${stroke}/>`, "#169c9c"),
    chat: svg(`<path d='M8 10h16v10H15l-5 4v-4H8z' fill='#fff'/>`, "#5865c8"),
    music: svg(`<path d='M13 21V10l9-2v11' ${stroke}/><circle cx='11' cy='21' r='2.4' fill='#fff'/><circle cx='20' cy='19' r='2.4' fill='#fff'/>`, "#1db36a"),
    game: svg(`<path d='M16 6l3 7 7 .8-5.3 4.7 1.6 7.2L16 22l-6.3 3.7 1.6-7.2L6 13.8 13 13z' fill='#fff'/>`, "#e8793a"),
  });
  const art = (h1, h2) =>
    "data:image/svg+xml;utf8," +
    encodeURIComponent(
      `<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'><defs><linearGradient id='g' x1='0' y1='0' x2='1' y2='1'><stop offset='0' stop-color='hsl(${h1} 75% 55%)'/><stop offset='1' stop-color='hsl(${h2} 80% 45%)'/></linearGradient></defs><rect width='64' height='64' fill='url(#g)'/><circle cx='44' cy='20' r='16' fill='hsl(${h2} 90% 75%)' opacity='.55'/><circle cx='18' cy='46' r='22' fill='hsl(${h1} 90% 30%)' opacity='.45'/></svg>`,
    );

  // ---------------------------------------------------------------- the apps on this PC
  const APPS = (W.APPS = {
    code: { name: "Visual Studio Code", exe: "code.exe", cat: "coding" },
    term: { name: "Terminal", exe: "windowsterminal.exe", cat: "coding" },
    browser: { name: "Browser", exe: "msedge.exe", cat: "browsing" },
    chat: { name: "Discord", exe: "discord.exe", cat: "communication" },
    music: { name: "Music", exe: "spotify.exe", cat: "media" },
    game: { name: "Starfall", exe: "starfall.exe", cat: "other" },
  });
  const byExe = (exe) => Object.keys(APPS).find((id) => APPS[id].exe === exe);
  const visible = () => Object.keys(APPS).filter((id) => W.win[id] && W.win[id].open && !W.win[id].min);

  // ---------------------------------------------------------------- CPU / RAM / GPU
  const sys = { cpu: 12, ram: 44, gpu: 4 };
  setInterval(() => {
    const t = now();
    const game = st.hasGame ? 1 : 0;
    const target = 9 + Math.sin(t / 3.1) * 4 + Math.random() * 6 + game * 24 + (NADIAudio.playing ? 3 : 0);
    sys.cpu += (target - sys.cpu) * 0.4;
    sys.ram += (43 + Math.sin(t / 40) * 2 + game * 12 - sys.ram) * 0.1;
    sys.gpu += (3 + Math.random() * 3 + game * 62 + (NADIAudio.playing ? 2 : 0) - sys.gpu) * 0.4;
  }, 500);
  C.get_sys_stats = () => ({ cpu_pct: sys.cpu, ram_pct: sys.ram, ram_used_gb: (sys.ram / 100) * 32, ram_total_gb: 32 });
  C.get_gpu_pct = () => sys.gpu;

  // ---------------------------------------------------------------- media: the Music app is a media session
  const media = (W.media = { idx: 0, playing: false, pos: 0, rate: 1, lastTrack: "" });
  const track = () => NADIAudio.TRACKS[media.idx];
  const mediaSnap = () => {
    const open = !!(W.win.music && W.win.music.open);
    if (!open) return { has_session: false, title: "", artist: "", playing: false, position: 0, duration: 0, can_previous: false, can_next: false, can_seek: false, can_rate: false, rate: 1, art: null, source: "" };
    const t = track();
    return {
      has_session: true,
      title: t.title,
      artist: t.artist,
      playing: media.playing,
      position: media.pos,
      duration: t.dur,
      can_previous: true,
      can_next: true,
      can_seek: true,
      can_rate: true,
      rate: media.rate,
      art: art(t.hue[0], t.hue[1]),
      source: "music",
    };
  };
  function mediaEmit() {
    const snap = mediaSnap();
    st.hasMedia = snap.has_session;
    const key = `${snap.title}|${snap.artist}`;
    if (snap.has_session && key !== media.lastTrack) st.peekRequest = true;
    media.lastTrack = snap.has_session ? key : "";
    NADI.emit("media-tick", snap);
    W.onMedia?.(snap);
  }
  W.mediaEmit = mediaEmit;
  W.mediaSnap = mediaSnap;
  function setPlaying(p) {
    media.playing = p;
    if (p) NADIAudio.play(media.idx, media.rate);
    else NADIAudio.pause();
    mediaEmit();
  }
  W.musicClosed = () => {
    media.playing = false;
    NADIAudio.pause();
    mediaEmit();
  };
  function skip(d) {
    media.idx = (media.idx + d + NADIAudio.TRACKS.length) % NADIAudio.TRACKS.length;
    media.pos = 0;
    NADIAudio.setTrack(media.idx);
    mediaEmit();
  }
  C.media_play_pause = () => setPlaying(!media.playing);
  C.media_next = () => skip(1);
  C.media_previous = () => {
    if (media.pos > 4) {
      media.pos = 0;
      mediaEmit();
    } else skip(-1);
  };
  C.media_seek = ({ positionSeconds }) => {
    media.pos = clamp(positionSeconds, 0, track().dur);
    mediaEmit();
  };
  C.media_seek_by = ({ deltaSeconds }) => {
    media.pos = clamp(media.pos + deltaSeconds, 0, track().dur);
    mediaEmit();
  };
  C.media_set_rate = ({ rate }) => {
    media.rate = clamp(rate, 0.1, 16);
    NADIAudio.setRate(media.rate);
    mediaEmit();
    return true;
  };
  setInterval(() => {
    if (media.playing && W.win.music && W.win.music.open) {
      media.pos += 0.1 * media.rate;
      if (media.pos >= track().dur) skip(1);
    }
  }, 100);
  setInterval(mediaEmit, 1000);

  // ---------------------------------------------------------------- the game
  const game = (W.game = { running: false, since: 0, pid: 4242, frames: [] });
  const gameSnap = () => ({ has_game: game.running && S().game_detection, name: "Starfall", icon: ICONS.game, count: game.running ? 1 : 0, pid: game.running ? game.pid : 0 });
  function gameEmit() {
    const snap = gameSnap();
    st.hasGame = snap.has_game;
    NADI.emit("game-tick", snap);
  }
  W.gameEmit = gameEmit;
  W.gameStart = () => {
    game.running = true;
    game.since = now();
    game.frames.length = 0;
    gameEmit();
  };
  W.gameStop = () => {
    game.running = false;
    gameEmit();
  };
  setInterval(gameEmit, 1500);
  C.get_game_stats = ({ pids }) =>
    pids
      .filter((p) => p === game.pid && game.running)
      .map((pid) => {
        const f = game.frames.slice(-120);
        const avg = f.length ? f.reduce((a, b) => a + b, 0) / f.length : 16.7;
        const sorted = [...f].sort((a, b) => a - b);
        const p99 = sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.99))] : avg;
        return {
          pid,
          cpu_pct: Math.min(100, 14 + Math.sin(now() / 2) * 4 + Math.random() * 3),
          ram_gb: 4.1 + Math.sin(now() / 30) * 0.05,
          ram_pct: 12.8,
          gpu_pct: clamp(58 + Math.sin(now() / 1.7) * 14 + Math.random() * 4, 0, 100),
          vram_gb: 3.4 + Math.sin(now() / 25) * 0.1,
          fps: f.length > 10 ? 1000 / avg : null,
          low_fps: f.length > 10 ? 1000 / p99 : null,
          frame_ms: f.slice(-60),
          fps_denied: false,
        };
      });

  // ---------------------------------------------------------------- editor + git state (the coding card)
  const code = (W.code = { file: "island.rs", unsaved: false, typedLines: 0 });
  const projectInfo = () => {
    const files = [
      { name: "island.rs", path: "src/island.rs", added: 42 + code.typedLines, deleted: 7, status: "M" },
      { name: "hub.css", path: "ui/hub.css", added: 28, deleted: 3, status: "M" },
      { name: "settings.rs", path: "src/settings.rs", added: 11, deleted: 0, status: "M" },
      { name: "notes.md", path: "notes.md", added: 9, deleted: 0, status: "A" },
    ];
    const added = files.reduce((s, f) => s + f.added, 0) + 31;
    const deleted = files.reduce((s, f) => s + f.deleted, 0) + 12;
    return { name: "nadi-demo", branch: "main", files, changed: 5, added, deleted };
  };
  const editorTitle = () => `${code.unsaved ? "● " : ""}${code.file} - nadi-demo - Visual Studio Code`;

  // ---------------------------------------------------------------- the browser page and downloads
  const page = (W.page = {
    title: "NADI downloads",
    domain: "nadi.dev",
    lead: "NADI is a small pill with a pair of eyes at the top of your screen. Grab the installer below, or the source, or the demo reel.",
    buttons: ["Download installer", "Download source", "Download demo reel"],
  });
  const FILES = {
    "Download installer": ["NADI_1.0.0_x64-setup.exe", 62_000_000, 5_400_000],
    "Download source": ["nadi-source.zip", 18_500_000, 2_100_000],
    "Download demo reel": ["nadi-demo-reel.mp4", 1_240_000_000, 9_800_000],
  };
  const dl = (W.dl = { items: [], n: 0 });
  W.startDownload = (label) => {
    const f = FILES[label];
    if (!f) return false;
    if (dl.items.some((i) => i.name === f[0] && !i.done)) return true;
    dl.items = dl.items.filter((i) => i.name !== f[0]);
    dl.items.push({ id: `dl${++dl.n}`, source: "Browser", kind: "browser", name: f[0], received: 0, total: f[1], speed: 0, done: false, exe_path: APPS.browser.exe, icon: ICONS.browser, base: f[2] });
    return true;
  };
  setInterval(() => {
    const t = now();
    for (const it of dl.items) {
      if (it.done) continue;
      it.speed = Math.max(300_000, it.base * (0.8 + 0.3 * Math.sin(t / 2 + it.base) + Math.random() * 0.2));
      it.received = Math.min(it.total, it.received + it.speed * 0.5);
      if (it.received >= it.total) {
        it.done = true;
        it.speed = 0;
      }
    }
  }, 500);
  const downloadCards = () => {
    if (!dl.items.length) return [];
    const items = dl.items.slice(-3).map((i) => ({ ...i }));
    return [{ speed: items.reduce((s, i) => s + (i.done ? 0 : i.speed), 0), items }];
  };
  C.download_item_click = ({ id }) => {
    dl.items = dl.items.filter((i) => i.id !== id);
  };
  C.click_page_button = ({ label }) => !!NADI.pressPageButton?.(label);

  // ---------------------------------------------------------------- Claude usage (the hub's two rings and the peek)
  const usage = (W.usage = { five: 38, seven: 61 });
  const usageSnap = () => ({
    available: true,
    error: null,
    five_hour: { pct: usage.five, resets_at: new Date(Date.now() + 2.2 * 3600e3).toISOString() },
    seven_day: { pct: usage.seven, resets_at: new Date(Date.now() + 3.2 * 86400e3).toISOString() },
  });
  C.get_usage = () => usageSnap();
  C.refresh_usage = () => NADI.emit("usage-tick", usageSnap());
  function bumpUsage(five, seven) {
    const old = [usage.five, usage.seven];
    usage.five = Math.min(99, usage.five + five);
    usage.seven = Math.min(99, usage.seven + seven);
    NADI.emit("usage-tick", usageSnap());
    // maybe_peek: not over the hub, not while something else is showing
    if (st.hubOpen || notif.hasCurrent()) return;
    NADI.emit("usage-peek", { before: { five_hour: old[0], seven_day: old[1] }, after: { five_hour: usage.five, seven_day: usage.seven } });
    st.usagePeekUntil = now() + 4.4;
  }
  W.bumpUsage = bumpUsage;

  // ---------------------------------------------------------------- calendar
  const cal = (() => {
    const d = new Date();
    const at = (days, h, m) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + days, h, m).getTime();
    const soon = Math.ceil((Date.now() + 25 * 60e3) / 300e3) * 300e3;
    return [
      { uid: "e1", summary: "Design review", start_ms: soon, end_ms: soon + 30 * 60e3, all_day: false },
      { uid: "e2", summary: "Lunch with Sam", start_ms: at(Date.now() > at(0, 12, 30) ? 1 : 0, 12, 30), end_ms: at(Date.now() > at(0, 12, 30) ? 1 : 0, 13, 30), all_day: false },
      { uid: "e3", summary: "Sprint planning", start_ms: at(1, 10, 0), end_ms: at(1, 11, 0), all_day: false },
      { uid: "e4", summary: "Dentist", start_ms: at(3, 15, 0), end_ms: at(3, 15, 45), all_day: false },
      { uid: "e5", summary: "Mia's birthday", start_ms: at(5, 0, 0), end_ms: at(6, 0, 0), all_day: true },
    ];
  })();
  W.calendar = cal;
  C.get_calendar = () => ({ configured: !!S().calendar_ics_url, error: null, events: S().calendar_ics_url ? cal : [] });
  const notified = new Set();
  setInterval(() => {
    if (!S().calendar_ics_url) return;
    const lead = S().calendar_reminder_lead_min * 60e3;
    for (const ev of cal) {
      const delta = ev.start_ms - Date.now();
      if (delta > 0 && delta <= lead && !notified.has(ev.uid)) {
        notified.add(ev.uid);
        notif.push("Upcoming event", `${ev.summary} in ${Math.max(1, Math.floor(delta / 60e3))} min`);
        NADI.pushHistory();
      }
    }
  }, 30000);

  // ---------------------------------------------------------------- the time announcement (clock.rs)
  const INTERVALS = [5, 10, 15, 30, 60, 120, 180, 360];
  function clockPush(h24) {
    const d = new Date();
    const time = h24
      ? `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`
      : `${d.getHours() % 12 || 12}:${String(d.getMinutes()).padStart(2, "0")} ${d.getHours() < 12 ? "AM" : "PM"}`;
    const date = d.toLocaleDateString("en-US", { weekday: "short", month: "short", day: "numeric" }).replace(/^(\w+)\s/, "$1, ");
    notif.pushBrief(time, { id: "time", state: "time", host_icon: "clock", host_exe: null, project: date, ctx: 0 });
  }
  C.time_preview = () => clockPush(S().time_24h);
  W.clockPush = clockPush;
  let lastMark = null;
  setInterval(() => {
    if (!S().time_announce) return;
    const every = INTERVALS[Math.min(S().time_interval, INTERVALS.length - 1)];
    const d = new Date();
    const minute = d.getHours() * 60 + d.getMinutes();
    const mark = `${d.getDate()}:${minute}`;
    if (minute % every !== 0 || lastMark === mark) return;
    lastMark = mark;
    if (st.hubOpen || st.hasGame) return;
    clockPush(S().time_24h);
  }, 10000);

  // ---------------------------------------------------------------- Claude Code sessions (llm.rs)
  const llm = (W.llm = {
    kind: "none", // none | busy | waiting | idle
    id: "demo-session",
    title: "Add a dark mode toggle",
    prompt: "",
    feed: [],
    activity: { icon: "sparkle", text: "Thinking" },
    started: 0,
    updated: 0,
    busySince: 0,
    tokens: 0,
    cost: 0,
    added: 0,
    removed: 0,
    dismissed: null,
    timers: [],
    turns: 0,
  });
  const llmClear = () => {
    for (const t of llm.timers) clearTimeout(t);
    llm.timers = [];
  };
  const llmAfter = (s, fn) => llm.timers.push(setTimeout(fn, s * 1000));
  const llmFeed = (icon, text) => {
    if (llm.activity && llm.kind === "busy" && llm.activity.text !== "Thinking") llm.feed.unshift(llm.activity);
    llm.feed = llm.feed.slice(0, 4);
    llm.activity = { icon, text };
    llm.updated = Date.now();
  };
  // a state change of the session: what llm.rs's watcher turns into a pill
  function llmKind(next) {
    const prev = llm.kind;
    if (prev === next) return;
    llm.kind = next;
    const ts = Date.now();
    if (prev === "waiting" && next !== "waiting") notif.cancelBrief(llm.id);
    if (next === "busy") llm.busySince = ts;
    llm.updated = ts;
    if (!(S().llm_detection && S().llm_brief)) return;
    if (next === "waiting") llmBrief("waiting");
    else if (prev === "busy" && next === "idle" && ts - llm.busySince >= 5000) {
      bumpUsage(3, 1);
      llmBrief("finished");
    }
  }
  function llmBrief(state) {
    if (st.hubOpen || st.hasGame) return; // the hub shows the cards already; a game is not interrupted
    if (W.fg === "term") return; // the terminal is in front: you are looking at it
    notif.pushBrief(llm.title, { id: llm.id, state, host_icon: "term", host_exe: APPS.term.exe, project: "nadi-demo", ctx: Math.min(1, llm.tokens / 200000) });
  }
  W.llmRun = (prompt) => {
    if (llm.kind === "busy" || llm.kind === "waiting") return;
    llmClear();
    llm.prompt = prompt;
    llm.title = "Add a dark mode toggle";
    llm.feed = [];
    llm.dismissed = null;
    if (!llm.started) llm.started = Date.now();
    llm.turns++;
    llm.tokens = Math.max(llm.tokens, 61000);
    llm.activity = { icon: "sparkle", text: "Thinking" };
    llmKind("busy");
    llmAfter(2, () => llmFeed("eye", "Reading src/settings.rs"));
    llmAfter(4, () => llmFeed("search", "Searching the code"));
    llmAfter(6, () => llmFeed("pen", "Editing ui/hub.css"));
    llmAfter(8, () => {
      llm.activity = { icon: "phone", text: "Waiting for your approval" };
      W.onLlm?.();
      llmKind("waiting");
    });
    W.onLlm?.();
  };
  W.llmAnswer = (yes) => {
    if (llm.kind !== "waiting") return;
    llmClear();
    if (!yes) {
      llm.activity = { icon: "moon", text: "Idle" };
      llmKind("idle");
      llm.updated = Date.now();
      W.onLlm?.();
      return;
    }
    llm.activity = { icon: "term", text: "Running cargo test" };
    llmKind("busy");
    llmAfter(3, () => llmFeed("pen", "Editing ui/main.js"));
    llmAfter(6, () => llmFeed("term", "Running cargo build --release"));
    llmAfter(10, () => {
      llm.activity = { icon: "check", text: "Finished" };
      llmKind("idle");
      W.onLlm?.();
    });
    W.onLlm?.();
  };
  W.llmReset = () => {
    llmClear();
    notif.cancelBrief(llm.id);
    llm.kind = "none";
    llm.feed = [];
    llm.tokens = 0;
    llm.cost = 0;
    llm.added = 0;
    llm.removed = 0;
    llm.started = 0;
    W.onLlm?.();
  };
  setInterval(() => {
    if (llm.kind === "busy") {
      llm.tokens += 1400 + Math.random() * 900;
      llm.cost += 0.012 + Math.random() * 0.01;
      if (Math.random() < 0.5) llm.added += 1 + Math.floor(Math.random() * 4);
      if (Math.random() < 0.2) llm.removed += 1;
    }
  }, 1000);
  const llmState = () => {
    if (llm.kind === "waiting") return "waiting";
    if (llm.kind === "busy") return "working";
    if (llm.kind === "idle" && llm.turns > 0 && llm.activity.icon === "check" && llm.dismissed !== llm.updated && Date.now() - llm.updated < 15 * 60e3) return "finished";
    return "idle";
  };
  W.llmState = llmState;
  const llmCards = () => {
    if (llm.kind === "none") return [];
    const state = llmState();
    if (state === "idle") return [];
    return [
      {
        id: llm.id,
        pid: 5150,
        provider: "claude",
        title: llm.title,
        project: "nadi-demo",
        host: "Terminal",
        host_icon: "term",
        host_exe: APPS.term.exe,
        state,
        activity: state === "waiting" ? { icon: "phone", text: "Waiting for your approval" } : state === "finished" ? { icon: "check", text: "Finished" } : llm.activity,
        context_tokens: Math.round(llm.tokens),
        context_limit: 200000,
        model: "Sonnet 5.5",
        running_secs: Math.floor((Date.now() - llm.started) / 1000),
        prompt: llm.prompt,
        feed: llm.feed.slice(),
        cost_usd: llm.cost,
        lines_added: llm.added,
        lines_removed: llm.removed,
      },
    ];
  };
  C.llm_dismiss = () => {
    llm.dismissed = llm.updated;
  };

  // ---------------------------------------------------------------- what is in front: the work pill and the cards (work.rs, activity.rs)
  const CONFIRM_S = 3;
  const LEAVE_GRACE_S = 8;
  const RECENT_ACTIVE_S = 30;
  const NON_SESSION = ["idle", "unknown", "other"];
  const LABELS = { coding: "Coding", writing: "Writing", design: "Designing", communication: "Messaging", media: "Watching/Listening", gaming: "Gaming", browsing: "Browsing", other: "On the Computer" };
  const recent = new Map(); // category -> { last, started, app, exe, id }
  let candidate = "idle";
  let candidateSince = now();
  let candidateApp = "";
  let confirmed = null;
  let confirmedApp = "";
  let confirmedSince = now();
  function observe(cat, appId) {
    if (cat === "other") return;
    const prev = recent.get(cat);
    const fresh = prev && now() - prev.last <= RECENT_ACTIVE_S;
    recent.set(cat, { last: now(), started: fresh ? prev.started : now(), app: APPS[appId].name, exe: APPS[appId].exe, id: appId });
  }
  function workPoll() {
    if (!S().work_detection) {
      if (confirmed !== null) {
        confirmed = null;
        candidate = "idle";
        st.hasWork = false;
        NADI.emit("work-tick", { has_session: false, category: "", label: "", app_name: "", started_at_secs: 0 });
      }
      return;
    }
    // every visible window keeps its category's card alive, focused or not
    const seen = [];
    for (const id of visible()) {
      const cat = APPS[id].cat;
      if (cat === "other" || seen.includes(cat)) continue;
      seen.push(cat);
      observe(cat, id);
    }
    let state;
    let app = "";
    if (W.fg && W.win[W.fg] && W.win[W.fg].open && !W.win[W.fg].min) {
      state = APPS[W.fg].cat;
      app = APPS[W.fg].name;
      observe(state, W.fg);
    } else {
      state = "other";
    }
    if (state !== candidate) {
      candidate = state;
      candidateSince = now();
      candidateApp = app;
    }
    const inSession = confirmed !== null && !NON_SESSION.includes(confirmed);
    const dwell = inSession && NON_SESSION.includes(state) ? LEAVE_GRACE_S : CONFIRM_S;
    if (now() - candidateSince >= dwell && confirmed !== state) {
      confirmed = state;
      confirmedApp = candidateApp;
      confirmedSince = now();
    }
    const category = confirmed || "idle";
    const has = !NON_SESSION.includes(category);
    st.hasWork = has;
    NADI.emit("work-tick", { has_session: has, category, label: LABELS[category] || "Away", app_name: confirmedApp, started_at_secs: now() - confirmedSince });
  }
  setInterval(workPoll, 1000);
  W.setFg = (id) => {
    W.fg = id;
  };

  C.get_activity = () => {
    const snap = { games: [], downloads: [], llm: [], work: [], coding: null };
    if (game.running) snap.games.push({ pid: game.pid, name: "Starfall", exe_path: APPS.game.exe, icon: ICONS.game, playtime_secs: now() - game.since });
    if (S().llm_detection) snap.llm = llmCards();
    if (S().download_detection) snap.downloads = downloadCards();
    if (!S().work_detection) return snap;
    const list = [...recent.entries()].filter(([, r]) => now() - r.last <= RECENT_ACTIVE_S).sort((a, b) => b[1].started - a[1].started);
    for (const [cat, r] of list) {
      if (cat === "coding") {
        const session = confirmed === "coding";
        const editor = recent.get("coding");
        snap.coding = {
          exe_path: APPS.code.exe,
          today_secs: 4320 + Math.floor(now() - confirmedSince),
          active: session,
          project: "nadi-demo",
          active_secs: session ? now() - confirmedSince : 0,
          file: code.file,
          language: "Rust",
          language_color: "#DEA584",
          unsaved: code.unsaved,
          project_info: projectInfo(),
        };
        void editor;
      } else {
        const browsing = cat === "browsing";
        snap.work.push({
          exe_path: r.exe,
          category: cat,
          app_name: r.app,
          icon: ICONS[r.id] || null,
          going_secs: now() - r.started,
          page: browsing ? page.title : null,
          browse: browsing
            ? {
                domain: page.domain,
                preview: S().page_preview ? { lead: page.lead, buttons: page.buttons.map((label, i) => ({ label, x: 40, y: 120 + i * 30 })) } : null,
              }
            : null,
        });
      }
    }
    return snap;
  };

  // ---------------------------------------------------------------- windows: which app a card or pill takes you to
  C.focus_source = ({ exePath, titleHint, sourceId }) => {
    const id = (exePath && byExe(exePath)) || (sourceId && (APPS[sourceId] ? sourceId : null)) || (titleHint === "nadi-demo" ? "code" : null);
    if (id) NADI.focusApp?.(id, true);
  };

  // ---------------------------------------------------------------- backgrounds: the visitor's own image or video
  async function pick(kind) {
    return new Promise((resolve, reject) => {
      const input = document.createElement("input");
      input.type = "file";
      input.accept = "image/*,video/mp4,video/webm";
      NADI.st.picking = true;
      input.addEventListener("change", () => {
        NADI.st.picking = false;
        const f = input.files && input.files[0];
        if (!f) return resolve(null);
        if (f.size > 60 * 1024 * 1024) return reject("That file is too large for the demo.");
        const url = `${URL.createObjectURL(f)}#/${kind}-${Date.now()}-${f.name.replace(/[\\/#?]/g, "_")}`;
        const s = { ...S(), ["bg_" + kind]: url };
        Object.assign(S(), s);
        resolve({ ...s });
      });
      input.addEventListener("cancel", () => {
        NADI.st.picking = false;
        resolve(null);
      });
      input.click();
    });
  }
  C.pick_background = ({ kind }) => pick(kind);
  C.clear_background = ({ kind }) => {
    S()["bg_" + kind] = "";
    return { ...S() };
  };

  // ---------------------------------------------------------------- go
  NADI.onStart = () => {
    mediaEmit();
    gameEmit();
    workPoll();
    NADI.emit("usage-tick", usageSnap());
    NADI.emit("calendar-tick", C.get_calendar());
  };
})();
