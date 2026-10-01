// The mock desktop the island sits on: a few small apps whose windows can be moved, minimised and closed, a
// taskbar and a tray. They are props: what matters is that the island sees them the way it sees a real PC
// (which window is in front, what plays, what downloads, which session needs you), through world.js.
(() => {
  "use strict";
  const { notif, st, cmds: C } = NADI;
  const W = NADI.world;
  const desk = document.getElementById("desk");
  const $ = (s, r = desk) => r.querySelector(s);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

  const wins = {};
  let zTop = 10;

  // ------------------------------------------------------------ windows
  function defaultRect(id) {
    const dw = desk.clientWidth;
    return (
      {
        code: [28, 84, 470, 270],
        term: [28, 366, 470, 222],
        music: [Math.max(520, dw - 28 - 330), 84, 330, 218],
        browser: [Math.min(540, Math.max(60, dw - 470)), 150, 450, 340],
        chat: [Math.min(580, Math.max(80, dw - 430)), 210, 400, 330],
      }[id] || [100, 100, 400, 300]
    );
  }

  function make(id, title, bodyHtml, init) {
    const [x, y, w, h] = defaultRect(id);
    const el = document.createElement("div");
    el.className = "win";
    el.dataset.id = id;
    el.style.cssText = `left:${x}px;top:${y}px;width:${w}px;height:${h}px`;
    el.innerHTML = `<div class="wbar"><img alt="" src="${W.ICONS[id]}"><span class="wtitle">${esc(title)}</span><button data-a="min" aria-label="Minimise">&#8211;</button><button data-a="close" aria-label="Close">&#10005;</button></div><div class="wbody">${bodyHtml}</div>`;
    desk.insertBefore(el, $(".taskbar"));
    wins[id] = el;
    W.win[id] = { open: true, min: false };
    // move by the title bar
    $(".wbar", el).addEventListener("pointerdown", (e) => {
      if (e.target.closest("button") || e.button !== 0) return;
      const r = desk.getBoundingClientRect();
      const ox = e.clientX - el.offsetLeft - r.left;
      const oy = e.clientY - el.offsetTop - r.top;
      const move = (m) => {
        el.style.left = `${Math.max(-w + 80, Math.min(desk.clientWidth - 80, m.clientX - r.left - ox))}px`;
        el.style.top = `${Math.max(0, Math.min(desk.clientHeight - 80, m.clientY - r.top - oy))}px`;
      };
      const up = () => {
        removeEventListener("pointermove", move);
        removeEventListener("pointerup", up);
      };
      addEventListener("pointermove", move);
      addEventListener("pointerup", up);
    });
    el.addEventListener("pointerdown", () => focus(id), true);
    $(".wbar", el).addEventListener("click", (e) => {
      const a = e.target.closest("button")?.dataset.a;
      if (a === "min") minimise(id);
      if (a === "close") close(id);
    });
    init?.(el);
    focus(id);
    return el;
  }

  function topVisible(except) {
    return Object.keys(wins)
      .filter((id) => id !== except && W.win[id].open && !W.win[id].min)
      .sort((a, b) => Number(wins[b].style.zIndex) - Number(wins[a].style.zIndex))[0];
  }
  function focus(id) {
    W.win[id].min = false;
    wins[id].hidden = false;
    wins[id].style.zIndex = ++zTop;
    W.setFg(id);
    paintFocus();
  }
  function paintFocus() {
    for (const [id, el] of Object.entries(wins)) el.classList.toggle("active", W.fg === id);
    for (const b of desk.querySelectorAll(".tbtn")) {
      const w = W.win[b.dataset.id];
      b.classList.toggle("open", !!(w && w.open));
      b.classList.toggle("fg", W.fg === b.dataset.id && !!(w && w.open && !w.min));
    }
    desk.classList.toggle("gaming", !!(W.win.game && W.win.game.open));
  }
  function minimise(id) {
    W.win[id].min = true;
    wins[id].hidden = true;
    if (W.fg === id) W.setFg(topVisible(id) || null);
    paintFocus();
  }
  function close(id) {
    W.win[id].open = false;
    W.win[id].min = false;
    wins[id].hidden = true;
    if (W.fg === id) W.setFg(topVisible(id) || null);
    if (id === "music") W.musicClosed();
    if (id === "game") {
      W.gameStop();
      stopGame();
    }
    if (id === "term") W.llmReset();
    paintFocus();
  }
  function openApp(id) {
    if (id === "game" && !wins.game) createGame();
    else if (!wins[id]) CREATE[id]();
    else if (!W.win[id].open) {
      W.win[id].open = true;
      focus(id);
      if (id === "music") W.mediaEmit();
      if (id === "game") startGame();
    } else focus(id);
    if (id === "music") W.mediaEmit();
  }
  NADI.focusApp = (id, open) => {
    if (!open && !(W.win[id] && W.win[id].open)) return;
    openApp(id);
  };

  // ------------------------------------------------------------ the editor
  const CODE = `use crate::settings::Settings;

/// how wide the island is in the current view
pub fn view_size(view: PillView, width: u64) -> (f64, f64) {
    let (w, h) = view.size();
    let k = width as f64 / COMPACT_BASE;
    match view {
        PillView::Hub => (hub_w, h),
        PillView::Notification | PillView::Brief => (width as f64, h),
        _ => (w * k, h),
    }
}
`;
  function createCode() {
    make(
      "code",
      "island.rs",
      `<div class="ed"><div class="tabs"><b>island.rs<i class="dot" hidden></i></b><span>settings.rs</span><span>hub.css</span></div><textarea spellcheck="false">${esc(CODE)}</textarea><div class="status"><span>&#9095; main</span><span>Rust</span><span class="pos">Ln 1</span><button class="dbtn" data-a="save">Save</button></div></div>`,
      (el) => {
        const ta = $("textarea", el);
        const dot = $(".dot", el);
        const title = $(".wtitle", el);
        const sync = () => {
          dot.hidden = !W.code.unsaved;
          title.textContent = `${W.code.unsaved ? "● " : ""}island.rs`;
        };
        ta.addEventListener("input", (e) => {
          W.code.unsaved = true;
          if (e.inputType === "insertLineBreak") W.code.typedLines++;
          sync();
        });
        const save = () => {
          W.code.unsaved = false;
          sync();
        };
        $('[data-a="save"]', el).addEventListener("click", save);
        ta.addEventListener("keydown", (e) => {
          if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
            e.preventDefault();
            save();
          }
        });
        ta.addEventListener("keyup", () => {
          const n = ta.value.slice(0, ta.selectionStart).split("\n").length;
          $(".pos", el).textContent = `Ln ${n}`;
        });
      },
    );
  }

  // ------------------------------------------------------------ the terminal and the Claude Code session
  const PROMPT = "add a dark mode toggle to the settings";
  function createTerm() {
    make(
      "term",
      "Terminal",
      `<div class="term"><div class="tlog"></div><div class="tctl"></div></div>`,
      (el) => {
        const render = () => {
          const L = W.llm;
          const log = $(".tlog", el);
          const ctl = $(".tctl", el);
          const lines = [`<p class="mut">nadi-demo $ claude</p>`];
          if (L.kind !== "none") {
            lines.push(`<p><b>&gt;</b> ${esc(L.prompt)}</p>`);
            for (const f of [...L.feed].reverse()) lines.push(`<p class="mut">&#9679; ${esc(f.text)}</p>`);
            if (L.kind === "busy") lines.push(`<p class="cur"><span class="spin"></span> ${esc(L.activity.text)}…</p>`);
            if (L.kind === "waiting") lines.push(`<p class="ask">&#9679; Allow <code>Bash(cargo test)</code>?</p>`);
            if (L.kind === "idle") lines.push(`<p class="ok">&#10003; ${esc(L.activity.text === "Finished" ? "Done: 3 files changed." : "Stopped.")}</p>`);
          }
          const html = lines.join("");
          if (log._h !== html) {
            log.innerHTML = html;
            log._h = html;
            log.scrollTop = log.scrollHeight;
          }
          const key = L.kind;
          if (ctl._k === key) return;
          ctl._k = key;
          if (key === "waiting") {
            ctl.innerHTML = `<div class="row"><button class="dbtn go" data-a="yes">Yes, allow</button><button class="dbtn" data-a="no">No</button></div>`;
          } else if (key === "busy") {
            ctl.innerHTML = `<p class="hint">NADI drops a pill when this session needs you, but not while you are looking at the terminal. <b>Click another window</b> (the editor, say) and wait.</p>`;
          } else {
            ctl.innerHTML = `<div class="row"><input value="${esc(PROMPT)}" spellcheck="false"><button class="dbtn go" data-a="run">${key === "none" ? "Run claude" : "Run again"}</button></div>`;
          }
        };
        el.addEventListener("click", (e) => {
          const a = e.target.closest("[data-a]")?.dataset.a;
          if (a === "run") W.llmRun($("input", el).value.trim() || PROMPT);
          if (a === "yes") W.llmAnswer(true);
          if (a === "no") W.llmAnswer(false);
          render();
        });
        W.onLlm = render;
        setInterval(render, 500);
        render();
      },
    );
  }

  // ------------------------------------------------------------ the music player (a media session)
  const fmt = (s) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;
  function createMusic() {
    make(
      "music",
      "Music",
      `<div class="mus"><img class="cover" alt=""><div class="mtxt"><b class="mt"></b><span class="ma"></span></div><div class="mseek"><i></i></div><div class="mtime"><span class="mp">0:00</span><span class="md">0:00</span></div><div class="row mctl"><button class="dbtn" data-a="prev" aria-label="Previous">&#9198;</button><button class="dbtn go" data-a="play">&#9654; Play</button><button class="dbtn" data-a="next" aria-label="Next">&#9197;</button><button class="dbtn" data-a="mute" aria-label="Mute">&#128266;</button></div></div>`,
      (el) => {
        W.onMedia = (m) => {
          if (!m.has_session) return;
          $(".mt", el).textContent = m.title;
          $(".ma", el).textContent = m.artist;
          $(".cover", el).src = m.art;
          $(".mseek i", el).style.width = `${(m.position / m.duration) * 100}%`;
          $(".mp", el).textContent = fmt(m.position);
          $(".md", el).textContent = fmt(m.duration);
          $('[data-a="play"]', el).innerHTML = m.playing ? "&#10074;&#10074; Pause" : "&#9654; Play";
        };
        el.addEventListener("click", (e) => {
          const a = e.target.closest("[data-a]")?.dataset.a;
          if (a === "play") C.media_play_pause();
          if (a === "prev") C.media_previous();
          if (a === "next") C.media_next();
          if (a === "mute") {
            NADIAudio.setMuted(!NADIAudio.muted);
            e.target.closest("button").innerHTML = NADIAudio.muted ? "&#128263;" : "&#128266;";
          }
        });
        $(".mseek", el).addEventListener("click", (e) => {
          const r = e.currentTarget.getBoundingClientRect();
          C.media_seek({ positionSeconds: ((e.clientX - r.left) / r.width) * W.mediaSnap().duration });
        });
        W.mediaEmit();
      },
    );
  }

  // ------------------------------------------------------------ the browser (a page with buttons, and downloads)
  function createBrowser() {
    make(
      "browser",
      "NADI downloads",
      `<div class="brw"><div class="url"><span>&#128274; nadi.dev/download</span></div><div class="page"><h3>${esc(W.page.title)}</h3><p>${esc(W.page.lead)}</p><div class="row wrap">${W.page.buttons.map((b) => `<button class="dbtn go" data-b="${esc(b)}">${esc(b)}</button>`).join("")}</div><div class="shelf"></div></div></div>`,
      (el) => {
        const shelf = $(".shelf", el);
        const render = () => {
          const items = W.dl.items.slice(-3);
          shelf.innerHTML = items
            .map((i) => `<div class="dli"><span>${esc(i.name)}</span><i><b style="width:${(i.received / i.total) * 100}%"></b></i><em>${i.done ? "Done" : `${(i.speed / 1e6).toFixed(1)} MB/s`}</em></div>`)
            .join("");
        };
        setInterval(render, 500);
        el.addEventListener("click", (e) => {
          const b = e.target.closest("[data-b]");
          if (b) {
            W.startDownload(b.dataset.b);
            render();
          }
        });
        NADI.pressPageButton = (label) => {
          const b = [...el.querySelectorAll("[data-b]")].find((x) => x.dataset.b === label);
          if (!b || !W.win.browser.open) return false;
          b.classList.add("pressed");
          setTimeout(() => b.classList.remove("pressed"), 500);
          W.startDownload(label);
          render();
          return true;
        };
      },
    );
  }

  // ------------------------------------------------------------ chat (sends notifications)
  const FRIENDS = [
    ["Ann", "are you coming to lunch?"],
    ["Bo", "did you see the new build?"],
    ["Cy", "ping me when you're free"],
    ["Di", "that was hilarious"],
    ["Ed", "can you review my PR"],
    ["Fay", "game tonight?"],
  ];
  let friendN = 0;
  function discordMessage() {
    const [who, text] = FRIENDS[friendN++ % FRIENDS.length];
    notif.pushAction(who, text, "discord");
    NADI.pushHistory();
    const log = $(".cmsgs", wins.chat || desk);
    if (log) {
      log.insertAdjacentHTML("beforeend", `<p><b>${who}</b> ${esc(text)}</p>`);
      log.scrollTop = log.scrollHeight;
    }
  }
  function createChat() {
    make(
      "chat",
      "Discord",
      `<div class="chat"><div class="cmsgs"><p class="mut">#general</p><p><b>Ann</b> morning!</p><p><b>You</b> hey, one sec</p></div><div class="row"><button class="dbtn go" data-a="msg">Make a friend message me</button></div><p class="hint">Messages arrive as notifications: after three unread ones they merge into “N new messages”. Click a banner to come back here.</p></div>`,
      (el) => {
        $('[data-a="msg"]', el).addEventListener("click", discordMessage);
      },
    );
  }

  // ------------------------------------------------------------ the game: a borderless full-screen window
  let gameRaf = 0;
  const CREATE = { code: createCode, term: createTerm, music: createMusic, browser: createBrowser, chat: createChat };
  function createGame() {
    const el = document.createElement("div");
    el.className = "win gamewin";
    el.dataset.id = "game";
    el.innerHTML = `<canvas></canvas><button class="gquit">Quit game (Esc)</button><div class="ghint">move the mouse to steer, click to shoot</div>`;
    desk.insertBefore(el, $(".taskbar"));
    wins.game = el;
    W.win.game = { open: true, min: false };
    $(".gquit", el).addEventListener("click", () => close("game"));
    el.addEventListener("pointerdown", () => focus("game"), true);
    focus("game");
    startGame();
  }
  function startGame() {
    W.gameStart();
    const el = wins.game;
    el.hidden = false;
    const cv = $("canvas", el);
    const g = cv.getContext("2d");
    const fit = () => {
      cv.width = desk.clientWidth;
      cv.height = desk.clientHeight;
    };
    fit();
    let mx = cv.width / 2;
    const shots = [];
    const rocks = [];
    const stars = Array.from({ length: 90 }, () => [Math.random() * cv.width, Math.random() * cv.height, Math.random() * 2 + 0.4]);
    let score = 0;
    let last = performance.now();
    let spawn = 0;
    let lastShot = 0;
    cv.onpointermove = (e) => {
      mx = e.clientX - cv.getBoundingClientRect().left;
    };
    cv.onpointerdown = () => shots.push([mx, cv.height - 90]);
    const loop = (t) => {
      const ms = t - last;
      last = t;
      const dt = Math.min(ms, 50) / 1000;
      W.game.frames.push(ms);
      if (W.game.frames.length > 300) W.game.frames.shift();
      g.fillStyle = "#07070d";
      g.fillRect(0, 0, cv.width, cv.height);
      g.fillStyle = "#fff";
      for (const s of stars) {
        s[1] = (s[1] + s[2] * 60 * dt) % cv.height;
        g.globalAlpha = 0.35 + s[2] / 3;
        g.fillRect(s[0], s[1], s[2], s[2]);
      }
      g.globalAlpha = 1;
      spawn -= dt;
      if (spawn <= 0) {
        rocks.push([Math.random() * cv.width, -30, 14 + Math.random() * 22, 80 + Math.random() * 140]);
        spawn = 0.45;
      }
      if (t - lastShot > 180) {
        shots.push([mx, cv.height - 90]);
        lastShot = t;
      }
      for (let i = shots.length - 1; i >= 0; i--) {
        shots[i][1] -= 520 * dt;
        if (shots[i][1] < -10) shots.splice(i, 1);
      }
      for (let i = rocks.length - 1; i >= 0; i--) {
        const r = rocks[i];
        r[1] += r[3] * dt;
        let dead = r[1] > cv.height + 40;
        for (let j = shots.length - 1; j >= 0; j--) {
          if (Math.hypot(shots[j][0] - r[0], shots[j][1] - r[1]) < r[2]) {
            shots.splice(j, 1);
            dead = true;
            score++;
            break;
          }
        }
        if (dead) rocks.splice(i, 1);
      }
      g.fillStyle = "#ff9a55";
      for (const r of rocks) {
        g.beginPath();
        g.arc(r[0], r[1], r[2], 0, 7);
        g.fill();
      }
      g.fillStyle = "#6ee7ff";
      for (const s of shots) g.fillRect(s[0] - 1.5, s[1], 3, 12);
      g.fillStyle = "#fff";
      g.beginPath();
      g.moveTo(mx, cv.height - 100);
      g.lineTo(mx - 16, cv.height - 62);
      g.lineTo(mx + 16, cv.height - 62);
      g.fill();
      g.font = "600 14px Segoe UI, sans-serif";
      g.fillText(`Starfall  ·  score ${score}`, 18, cv.height - 62);
      gameRaf = requestAnimationFrame(loop);
    };
    cancelAnimationFrame(gameRaf);
    gameRaf = requestAnimationFrame(loop);
  }
  function stopGame() {
    cancelAnimationFrame(gameRaf);
  }
  addEventListener("keydown", (e) => {
    if (e.key === "Escape" && W.win.game && W.win.game.open) close("game");
  });

  // ------------------------------------------------------------ the taskbar and the tray
  const bar = document.createElement("div");
  bar.className = "taskbar";
  bar.innerHTML = `<div class="tapps">${["code", "term", "browser", "chat", "music", "game"]
    .map((id) => `<button class="tbtn" data-id="${id}" title="${esc(W.APPS[id].name)}"><img alt="" src="${W.ICONS[id]}"><i></i></button>`)
    .join("")}</div><div class="ttray"><button class="tnadi" aria-label="NADI tray icon: click for the hub, right-click for the menu" title="NADI"><svg viewBox="0 0 28 16"><rect x="1" y="1" width="26" height="14" rx="7" fill="#000" stroke="#555"/><rect x="8" y="4.5" width="3" height="7" rx="1.5" fill="#fff"/><rect x="17" y="4.5" width="3" height="7" rx="1.5" fill="#fff"/></svg></button><span class="tclock"></span></div><div class="tmenu" hidden><button data-m="hub">Open Hub</button><button data-m="settings">Settings</button><button data-m="show">Show Island</button><hr><button data-m="quit">Quit</button></div>`;
  desk.append(bar);
  bar.addEventListener("click", (e) => {
    const b = e.target.closest(".tbtn");
    if (b) {
      const id = b.dataset.id;
      if (W.win[id] && W.win[id].open && !W.win[id].min && W.fg === id && id !== "game") minimise(id);
      else openApp(id);
    }
  });
  const menu = $(".tmenu");
  const tray = $(".tnadi");
  tray.addEventListener("click", () => {
    menu.hidden = true;
    NADI.openHubFromTray();
  });
  tray.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    menu.hidden = !menu.hidden;
  });
  menu.addEventListener("click", (e) => {
    const m = e.target.closest("button")?.dataset.m;
    menu.hidden = true;
    if (m === "hub") NADI.openHubFromTray();
    if (m === "settings") {
      NADI.emit("open-settings", null);
      NADI.openHubFromTray();
    }
    if (m === "show") st.shown = true;
    if (m === "quit") toast("Quit is switched off in the demo. In NADI it closes the app.");
  });
  document.addEventListener("pointerdown", (e) => {
    if (!e.target.closest(".tmenu, .tnadi")) menu.hidden = true;
  });
  // the desktop itself: clicking it puts nothing in front
  $(".wall").addEventListener("pointerdown", () => {
    W.setFg(null);
    paintFocus();
  });
  const tclock = $(".tclock");
  const tickClock = () => {
    const d = new Date();
    tclock.textContent = d.toLocaleTimeString([], NADI.settings().time_24h ? { hour: "2-digit", minute: "2-digit", hourCycle: "h23" } : { hour: "numeric", minute: "2-digit" });
  };
  setInterval(tickClock, 1000);
  tickClock();

  let toastTimer = 0;
  function toast(text) {
    const t = $(".dtoast");
    t.textContent = text;
    t.classList.add("on");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.remove("on"), 3200);
  }

  // ------------------------------------------------------------ shortcuts used by the page around the desktop
  NADI.demo = {
    discord: discordMessage,
    viber() {
      notif.pushAction("Viber", "New message", "viber");
      NADI.pushHistory();
    },
    mail() {
      notif.push("Mail", "Your order has shipped");
      NADI.pushHistory();
    },
    reminder() {
      notif.push("Upcoming event", "Design review in 15 min");
      NADI.pushHistory();
    },
    time: () => C.time_preview(),
    hub: () => NADI.openHubFromTray(),
    settings() {
      NADI.emit("open-settings", null);
      NADI.openHubFromTray();
    },
    open: openApp,
    toast,
  };

  // ------------------------------------------------------------ the starting layout: an editor, a terminal and a music player
  createCode();
  createTerm();
  createMusic();
  focus("code");
  paintFocus();
})();
