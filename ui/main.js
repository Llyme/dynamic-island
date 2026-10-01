// Canvas-painted idle "eyes" -- a straight port of `_paint_eyes`/`_tick_eyes`
// from the Qt prototype (main.py), driven by the Rust-side `cursor-tick`
// event instead of a QTimer polling QCursor.pos(). No framework, no bundler:
// this is the whole idle-state UI, kept as cheap as the old QPainter path.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const pill = document.getElementById("pill");
const canvas = document.getElementById("face");
const ctx = canvas.getContext("2d");
const hubEyesCanvas = document.getElementById("hub-eyes");
const hubEyesCtx = hubEyesCanvas.getContext("2d");

let dpr = window.devicePixelRatio || 1;

function sizeCanvasToElement(c) {
  // Only sets the drawing-buffer resolution (width/height attributes) --
  // never the inline style size. CSS alone controls the displayed size
  // (#face and #hub-eyes both already size themselves via their own rules);
  // writing inline style here bit us once already: #hub-eyes was
  // `display:none` (0x0) the first time this ran at page load, and setting
  // `style.height = "0px"` inline then permanently overrode its CSS
  // `height: 40px` rule -- inline style always wins over a class -- so it
  // stayed invisible forever after, even once re-measured on becoming visible.
  // layout size, not getBoundingClientRect: that one includes CSS transforms (the shrunken pinned
  // island, entrance animations), and the buffer must not follow those
  const width = c.offsetWidth;
  const height = c.offsetHeight;
  c.width = Math.round(width * dpr);
  c.height = Math.round(height * dpr);
}

function resizeCanvas() {
  dpr = window.devicePixelRatio || 1;
  sizeCanvasToElement(canvas);
  sizeCanvasToElement(hubEyesCanvas);
}
window.addEventListener("resize", resizeCanvas);
resizeCanvas();

// -- eye state: blink + squash/stretch (ported from the Qt prototype) plus a
// small mood system (happy / sleepy / wide) driven by springs, so expression
// changes overshoot and settle like everything else in the island --
const eyeState = {
  blinkState: "open", // "open" | "closing" | "opening"
  blinkStart: 0,
  nextBlinkAt: performance.now() + rand(2500, 5500),
  openness: 1.0,
  squish: 0.0,
  squishDir: [0, 0],
  lastCursor: { x: 0, y: 0 },
  smoothedCursor: { x: 0, y: 0 },
  doubleBlink: false,
  winkSide: 0, // -1 left eye only, +1 right eye only, 0 both
};

const mood = {
  happy: { p: 0, v: 0 },
  sleepy: { p: 0, v: 0 },
  wide: { p: 0, v: 0 },
  xd: { p: 0, v: 0 }, // > <  (excited, punchy music)
  bliss: { p: 0, v: 0 }, // u u  (chill)
  hover: false, // cursor is over the island
  happyUntil: 0,
  wideUntil: 0,
  lastMoveAt: performance.now(),
  // idle glances: after the cursor sits still a while the eyes look around
  glance: { x: 0, y: 0 },
  glanceTarget: { x: 0, y: 0 },
  glanceW: 0,
  nextGlanceAt: 0,
  lastFrame: performance.now(),
  force: null,
};
window.__mood = mood;

// -- listening: Rust analyzes the system audio (WASAPI loopback) and sends
// loudness, beat pulses and a rough speech-vs-music call. Music -> the eyes
// bob and sway on the beat (arches flash on strong hits); speech -> they
// "talk", opening and closing with the voice. --
const audio = {
  target: 0,
  level: 0,
  mode: "silent", // "silent" | "music" | "speech" (hard call, for logic that needs one)
  lastTick: 0,
  kind: "silent",
  voice: 0, // raw 0..1 how voice-like (jumps around on vocal music)
  voiceSm: 0, // what the visuals use: eased toward the director's call (see below)
  bang: 0, // 0..1 fast, hard, punchy music (picks the > < expression)
  bands: [0, 0, 0, 0], // smoothed bass / low-mid / mid / high, each 0..1
  bandTarget: [0, 0, 0, 0],
  bpm: 100,
  conf: 0,
  lastBeatAt: 0,
  energy: 0, // 0..1 how energetic the sound is (loudness, hit sharpness, tempo)
  chill: 0, // 0..1 how relaxed: soft, slow, gentle music
  pan: 0, // -1 left .. 1 right, smoothed: where the sound is coming from
  panTarget: 0,
  dance: 0, // level * (1 - voice): how much "music energy" to show
  talk: 0, // level * voice
  talkEnv: 0, // fast envelope for the eyes' open/close
  kick: { p: 0, v: 0 },
  nod: { p: 0, v: 0 }, // head-bang nod: snaps down on each kick-drum hit
  lastKickAt: 0,
  sway: 0, // phase, advances with the estimated tempo
};
// -- the "director": once a second it takes a sample of the audio and calls a
// vibe: silent / talk / chill / groove / bang. The call is not made from that
// one second alone but from a rolling ~20 s PROFILE of the current track (tempo
// feel, how often the kick lands, energy, vocal share...), which is far steadier
// than any single second. A new call must also hold for two samples in a row.
// The profile resets when the song changes, and finished profiles are
// remembered per song, so a replay is understood from its first second. --
const PROFILE_SECS = 20;
const PROFILE_KEY = "di.trackProfiles";
const newAcc = () => ({ n: 0, voice: 0, energy: 0, level: 0, bass: 0, beats: 0, kicks: 0 });
const director = {
  state: "silent",
  cand: "silent",
  count: 0,
  since: 0,
  lastSample: 0,
  video: false, // what plays looks like a video/film (set from the media session)
  acc: newAcc(),
  history: [], // one-second summaries of the current track, oldest first
  track: "", // "title|artist" of what is playing
};

let savedProfiles = {};
try {
  savedProfiles = JSON.parse(localStorage.getItem(PROFILE_KEY) || "{}");
} catch (_) {}
let profilesSavedAt = 0;

function avgHistory(list) {
  const o = { voice: 0, energy: 0, level: 0, bass: 0, beats: 0, kicks: 0 };
  for (const h of list) for (const k in o) o[k] += h[k];
  const n = Math.max(1, list.length);
  for (const k in o) o[k] /= n;
  return o;
}

function rememberProfile(P) {
  if (!director.track) return;
  delete savedProfiles[director.track]; // re-insert = most recent
  savedProfiles[director.track] = P;
  const keys = Object.keys(savedProfiles);
  for (let i = 0; i < keys.length - 200; i++) delete savedProfiles[keys[i]];
  const now = performance.now();
  if (now - profilesSavedAt > 15000) {
    profilesSavedAt = now;
    try {
      localStorage.setItem(PROFILE_KEY, JSON.stringify(savedProfiles));
    } catch (_) {}
  }
}

function onTrackChange(key) {
  director.track = key;
  // a known song starts from what we learned last time; a new one starts empty
  const known = savedProfiles[key];
  director.history = known ? Array.from({ length: 8 }, () => ({ ...known })) : [];
  director.since = 0; // no dwell wait: the new song may deserve a different vibe at once
}

listen("media-tick", (e) => {
  const m = e.payload;
  // video context: a video player, or anything long enough to be an episode/film
  // rather than a song. Mostly dialogue -- the eyes must not "vibe" to it.
  director.video =
    !!m && !!m.has_session && ((m.duration || 0) > 900 || /potplayer|vlc|mpv|mpc|wmplayer|kmplayer|gom|video|movies|netflix|primevideo|disney|hulu/i.test(m.source || ""));
  if (!m || !m.has_session) return;
  const key = `${m.title || ""}|${m.artist || ""}`;
  if (key !== director.track) onTrackChange(key);
});

function directorSample(now, alive) {
  const a = director.acc;
  director.acc = newAcc();
  const secs = Math.max(0.5, (now - director.lastSample) / 1000);
  director.lastSample = now;
  const n = a.n;
  let cand = "silent";
  if (alive && n > 0 && a.level / n >= 0.05) {
    director.history.push({
      voice: a.voice / n,
      energy: a.energy / n,
      level: a.level / n,
      bass: a.bass / n,
      beats: a.beats / secs,
      kicks: a.kicks / secs,
    });
    while (director.history.length > PROFILE_SECS) director.history.shift();
    const P = avgHistory(director.history);
    const recentVoice = avgHistory(director.history.slice(-4)).voice;
    const cur = director.state;
    if (director.video) {
      // movies / episodes: mostly dialogue over ambience. Lean hard toward "talk"
      // (the analyzer under-scores speech mixed with music and effects) and
      // otherwise stay "calm": no dancing, no rings, no head-banging.
      cand = recentVoice > (cur === "talk" ? 0.12 : 0.2) ? "talk" : "calm";
    } else if (recentVoice > (cur === "talk" ? 0.3 : 0.5)) cand = "talk";
    // punchy: a kick lands steadily, it is energetic and loud (vocals are fine -- the kick detector ignores them)
    else if (P.kicks >= (cur === "bang" ? 1.2 : 1.5) && P.energy >= (cur === "bang" ? 0.25 : 0.3) && P.level > 0.5) cand = "bang";
    else if (P.energy < (cur === "chill" ? 0.27 : 0.2) && P.beats < 1.4) cand = "chill";
    else cand = "groove";
    if (director.history.length >= 12) rememberProfile(P);
  }
  if (cand === director.state) {
    director.count = 0;
    return;
  }
  if (cand === "silent") {
    // silence is not ambiguous: no waiting
    director.state = "silent";
    director.count = 0;
    director.since = now;
    return;
  }
  director.count = cand === director.cand ? director.count + 1 : 1;
  director.cand = cand;
  if (director.count >= 2 && now - director.since >= 2000) {
    director.state = cand;
    director.count = 0;
    director.since = now;
  }
}

listen("audio-tick", (e) => {
  const a = e.payload;
  director.acc.n++;
  director.acc.voice += a.voice;
  director.acc.energy += a.energy;
  director.acc.level += a.level;
  director.acc.bass += a.bass;
  if (a.beat) director.acc.beats++;
  if (a.kick) director.acc.kicks++;
  audio.target = a.level;
  audio.kind = a.kind;
  audio.voice = a.voice;
  audio.bandTarget = [a.bass, a.lowmid, a.mid, a.high];
  audio.bpm = a.bpm;
  audio.conf = a.conf;
  audio.panTarget = a.pan;
  audio.energy = a.energy;
  audio.lastTick = performance.now();
  // head-bang: only a low-end kick-drum hit moves the head (vocals and melody
  // onsets don't), and the head is back up before the next hit
  if (a.kick && audio.bang > 0.25 && audio.lastTick - audio.lastKickAt > 170) {
    audio.lastKickAt = audio.lastTick;
    audio.nod.v += 40 * audio.bang;
  }
  // one reaction per hit: never re-trigger within 220 ms of the last one
  if (a.beat && a.level > 0.15 && audio.lastTick - audio.lastBeatAt > 220) {
    audio.lastBeatAt = audio.lastTick;
    // every beat bounces the eyes and sends a ripple through the island
    // chill music barely bounces: hits are scaled down by how relaxed it is
    audio.kick.v += (14 + 8 * a.level) * (1 - 0.88 * audio.chill) * (1 - 0.9 * audio.bang);
  }
  // sound-wave rings follow real onsets: strength = how hard that hit landed
  if (a.hit > 0.1 && audio.chill < 0.55 && director.state !== "calm") {
    vizBeat(0.12 + 0.88 * a.hit * (0.45 + 0.55 * a.level), director.state === "talk" ? 1 : 0, a.pan);
  }
});

function smoothTo(cur, target, dt, up, down) {
  return cur + (target - cur) * (1 - Math.exp(-dt * (target > cur ? up : down)));
}

function updateAudio(now, dt) {
  const alive = now - audio.lastTick < 700;
  const tgt = alive ? audio.target : 0;
  audio.level = smoothTo(audio.level, tgt, dt, 30, 8);
  for (let i = 0; i < 4; i++) {
    audio.bands[i] = smoothTo(audio.bands[i], alive ? audio.bandTarget[i] : 0, dt, 40, 10);
  }
  audio.pan = smoothTo(audio.pan, alive ? audio.panTarget : 0, dt, 18, 6);
  if (!alive) {
    director.acc = newAcc();
    if (director.state !== "silent") {
      director.state = "silent";
      director.since = now;
    }
  } else if (now - director.lastSample >= 1000) {
    directorSample(now, alive);
  }
  const vibe = director.state;
  audio.voiceSm = smoothTo(audio.voiceSm, vibe === "talk" ? 1 : 0, dt, 3, 2.5);
  const voice = audio.voiceSm;
  const chillTarget = vibe === "chill" ? 1 : 0;
  audio.bang = smoothTo(audio.bang, vibe === "bang" ? 1 : 0, dt, 3.5, 2.5);
  audio.chill = smoothTo(audio.chill, chillTarget, dt, 1.2, 3);
  // while chilling, a slow soft ring breathes out every few seconds instead of hit-driven ones
  if (audio.chill > 0.5 && audio.level > 0.1 && now - viz.lastBreath > 3000) {
    viz.lastBreath = now;
    pushRing(now, 0.3, viz.hue + 20, audio.pan);
  }
  audio.mode = !alive || audio.level < 0.05 ? "silent" : vibe === "talk" ? "speech" : "music";
  // "calm" (video without much speech right now): a whisper of movement only
  audio.dance = smoothTo(audio.dance, audio.level * (1 - voice) * (vibe === "calm" ? 0.2 : 1), dt, 12, 4);
  audio.talk = smoothTo(audio.talk, audio.level * voice, dt, 12, 4);
  // fast envelopes: the eyes follow syllables and hits, not just loudness
  const env = 0.5 * audio.level + 0.5 * audio.bands[2];
  audio.talkEnv = smoothTo(audio.talkEnv, env, dt, 60, 16);
  // head-bang nod spring: quick down, settles well inside one beat
  audio.nod.v += (420 * -audio.nod.p - 26 * audio.nod.v) * dt;
  audio.nod.p += audio.nod.v * dt;
  // beat kick: an underdamped spring, so each hit bounces and settles
  const acc = 260 * -audio.kick.p - 15 * audio.kick.v;
  audio.kick.v += acc * dt;
  audio.kick.p += audio.kick.v * dt;
  // sway at half the beat rate (a nod every other beat), locked to the tempo estimate
  if (audio.dance > 0.04) audio.sway += dt * Math.PI * (audio.bpm / 60) * (1 - 0.55 * audio.chill);
}

function rand(min, max) {
  return min + Math.random() * (max - min);
}

function springTo(s, target, dt) {
  // underdamped on purpose: an expression change pops past its target and settles
  const a = 340 * (target - s.p) - 19 * s.v;
  s.v += a * dt;
  s.p += s.v * dt;
}

// Which expression the music calls for. Picked when the director's vibe
// changes and then re-rolled only every 5-9 s, so it reads as a deliberate
// mood, never a flicker.
const emote = { name: "none", vibe: "", until: 0 };
function pickEmote(now) {
  if (emote.forced) return;
  if (director.state === emote.vibe && now < emote.until) return;
  emote.vibe = director.state;
  const options = {
    bang: ["xd", "arch", "xd", "arch", "open"],
    chill: ["bliss", "sleepy", "bliss", "open"],
    groove: ["open", "open", "arch"],
  }[emote.vibe] || ["none"];
  let next = options[Math.floor(Math.random() * options.length)];
  if (next === emote.name && options.length > 1) next = options[(options.indexOf(next) + 1) % options.length];
  emote.name = next;
  emote.until = now + rand(5000, 9000);
}

window.__setEmote = (name) => {
  // debug hook: pin an expression ("xd" | "bliss" | "arch" | "sleepy" | "open")
  emote.name = name;
  emote.vibe = director.state;
  emote.until = Infinity;
  emote.forced = true;
};

function updateMood(now) {
  const dt = Math.min((now - mood.lastFrame) / 1000, 0.05);
  mood.lastFrame = now;
  const idleFor = now - mood.lastMoveAt;
  const inIdle = currentView === "idle";

  const f = mood.force; // debug override (window.__mood.force = "happy" | "sleepy" | "wide")
  const wide = f === "wide" || (!f && now < mood.wideUntil) ? 1 : 0;
  const happy = f === "happy" || (!f && !wide && ((mood.hover && inIdle) || now < mood.happyUntil)) ? 1 : 0;
  const sleepy = f === "sleepy" || (!f && !wide && !happy && idleFor > 25000 && audio.mode === "silent") ? 1 : 0;
  springTo(mood.wide, wide, dt);
  // music-driven expressions (never over a hover / click reaction)
  pickEmote(now);
  const free = !happy && !wide && !f;
  const e = free ? emote.name : "none";
  springTo(mood.happy, Math.max(happy, e === "arch" ? 1 : 0), dt);
  springTo(mood.xd, e === "xd" ? 1 : 0, dt);
  springTo(mood.bliss, e === "bliss" ? 1 : 0, dt);
  // relaxed, half-lidded eyes while the music is chill
  springTo(mood.sleepy, Math.max(sleepy, e === "sleepy" ? 0.75 : 0.3 * audio.chill * (e === "bliss" ? 0 : 1)), dt);

  // glances: cursor parked for 4s+ -> look around now and then
  const glancing = idleFor > 4000 && !happy && !wide;
  if (glancing && now >= mood.nextGlanceAt) {
    const spots = [[-1, 0.1], [1, 0.1], [0.7, -0.8], [-0.7, -0.8], [0, 0], [-0.9, 0.5], [0.9, 0.5]];
    const [gx, gy] = spots[Math.floor(Math.random() * spots.length)];
    mood.glanceTarget = { x: gx, y: gy };
    mood.nextGlanceAt = now + rand(1400, 3400);
  }
  const k = 1 - Math.exp(-dt * 14);
  mood.glance.x += (mood.glanceTarget.x - mood.glance.x) * k;
  mood.glance.y += (mood.glanceTarget.y - mood.glance.y) * k;
  mood.glanceW += ((glancing ? 1 : 0) - mood.glanceW) * (1 - Math.exp(-dt * 8));
}

function tickBlink(now) {
  // no blinking while the face is doing an expression (> <, ^ ^, u u, sleepy lids):
  // it only blinks in its normal open-eyed mode. The next blink is pushed out so
  // it does not fire the instant the expression ends.
  const expressive = Math.max(mood.happy.p, mood.xd.p, mood.bliss.p, mood.sleepy.p) > 0.35;
  if (expressive && eyeState.blinkState === "open") {
    eyeState.nextBlinkAt = Math.max(eyeState.nextBlinkAt, now + 1200);
    return;
  }
  const slow = 1 + Math.max(0, Math.min(mood.sleepy.p, 1)) * 1.8; // sleepy blinks are lazy
  if (eyeState.blinkState === "open" && now >= eyeState.nextBlinkAt) {
    eyeState.blinkState = "closing";
    eyeState.blinkStart = now;
    // now and then: a wink instead of a blink
    if (!eyeState.doubleBlink && mood.happy.p < 0.3 && mood.sleepy.p < 0.3 && Math.random() < 0.07) {
      eyeState.winkSide = Math.random() < 0.5 ? -1 : 1;
    } else if (!eyeState.doubleBlink) {
      eyeState.winkSide = 0;
    }
  } else if (eyeState.blinkState === "closing") {
    const t = (now - eyeState.blinkStart) / (80 * slow);
    if (t >= 1) {
      eyeState.blinkState = "opening";
      eyeState.blinkStart = now;
      eyeState.openness = 0;
    } else {
      eyeState.openness = 1 - t;
    }
  } else if (eyeState.blinkState === "opening") {
    const hold = eyeState.winkSide !== 0 ? 260 : 0; // a wink lingers
    const t = (now - eyeState.blinkStart - hold) / (120 * slow);
    if (t < 0) {
      eyeState.openness = 0;
    } else if (t >= 1) {
      eyeState.blinkState = "open";
      eyeState.openness = 1;
      if (!eyeState.doubleBlink && eyeState.winkSide === 0 && Math.random() < 0.22) {
        eyeState.doubleBlink = true; // quick second blink
        eyeState.nextBlinkAt = now + 110;
      } else {
        eyeState.doubleBlink = false;
        eyeState.winkSide = 0;
        eyeState.nextBlinkAt = now + (mood.sleepy.p > 0.5 ? rand(1800, 3400) : rand(2500, 5500));
      }
    } else {
      eyeState.openness = t;
    }
  }
}

// gaze vector (each axis -1..1) from one eye toward the cursor, blended with
// the idle-glance target when the cursor has been still for a while
function gazeFor(eyeX, eyeY) {
  const cx = Math.max(-1, Math.min(1, (eyeState.smoothedCursor.x - eyeX) / 60));
  const cy = Math.max(-1, Math.min(1, (eyeState.smoothedCursor.y - eyeY) / 40));
  const w = mood.glanceW;
  return [cx + (mood.glance.x - cx) * w, cy + (mood.glance.y - cy) * w];
}

// Each eye is a closed shape built from two curves over x in [-1, 1]:
//   centerY(x) = c0 + k*x^2         (k > 0 bends the middle up: an arch)
//   top/bottom = centerY -/+ amp * sqrt(1 - x^2)
// A circle is c0=k=0 with top=bot=r; every mood is just a different set of
// numbers, so morphing between expressions is plain interpolation.
const EYE_SHAPES = {
  //            half-width, top, bottom, c0,   k
  neutral: { rx: 5.1, top: 5.8, bot: 5.8, c0: 0, k: 0, pw: 2 },
  happy: { rx: 6.0, top: 2.0, bot: 2.0, c0: -2.6, k: 5.2, pw: 2 }, // ^  ^ smiling arches
  sleepy: { rx: 5.6, top: 0.6, bot: 5.0, c0: -2.0, k: 0, pw: 2 }, // ◡  ◡ heavy lids
  wide: { rx: 4.6, top: 8.0, bot: 8.0, c0: 0, k: 0, pw: 2 }, // tall, startled ovals
  // >  < squeezed-shut chevrons: a sharp arch (pw ~ 1) drawn turned 90 degrees
  xd: { rx: 6.8, top: 1.5, bot: 1.5, c0: -3.5, k: 7.0, pw: 1.1 },
  // u  u blissfully closed
  bliss: { rx: 6.0, top: 2.0, bot: 2.0, c0: 2.6, k: -5.2, pw: 2 },
};
const EYE_KEYS = ["rx", "top", "bot", "c0", "k", "pw"];

function eyeParams(happy, sleepy, wide, xd = 0, bliss = 0) {
  const out = {};
  for (const key of EYE_KEYS) {
    const n = EYE_SHAPES.neutral[key];
    out[key] =
      n +
      happy * (EYE_SHAPES.happy[key] - n) +
      sleepy * (EYE_SHAPES.sleepy[key] - n) +
      wide * (EYE_SHAPES.wide[key] - n) +
      xd * (EYE_SHAPES.xd[key] - n) +
      bliss * (EYE_SHAPES.bliss[key] - n);
  }
  out.top = Math.max(out.top, 0.4); // springs overshoot; never invert a curve
  out.bot = Math.max(out.bot, 0.4);
  return out;
}

const EYE_STEPS = 32;

function eyePath(c, p, blink) {
  c.beginPath();
  for (let i = 0; i <= EYE_STEPS; i++) {
    const x = -1 + (2 * i) / EYE_STEPS;
    const s = Math.sqrt(Math.max(0, 1 - x * x));
    const y = p.c0 + p.k * Math.pow(Math.abs(x), p.pw) - p.top * blink * s;
    if (i === 0) c.moveTo(x * p.rx, y);
    else c.lineTo(x * p.rx, y);
  }
  for (let i = EYE_STEPS; i >= 0; i--) {
    const x = -1 + (2 * i) / EYE_STEPS;
    const s = Math.sqrt(Math.max(0, 1 - x * x));
    c.lineTo(x * p.rx, p.c0 + p.k * Math.pow(Math.abs(x), p.pw) + p.bot * blink * s);
  }
  c.closePath();
}

// Draws the eyes centered at (centerX, cy) on the given context -- shared by
// the idle pill's canvas and the hub header. `opts.scale` enlarges them.
function paintEyesAt(targetCtx, centerX, cy, opts = {}) {
  const sc = opts.scale || 1;
  const wide = Math.max(0, Math.min(mood.wide.p, 1.3));
  const happy = Math.max(0, Math.min(mood.happy.p, 1.25));
  const sleepy = Math.max(0, Math.min(mood.sleepy.p, 1.1));
  const xd = Math.max(0, Math.min(mood.xd.p, 1.15));
  const bliss = Math.max(0, Math.min(mood.bliss.p, 1.15));
  const [dirx, diry] = eyeState.squishDir;
  // motion squash & stretch stays axis-aligned (no rotation): rotating the eye
  // made blinks close sideways when the cursor moved vertically
  const ax = Math.abs(dirx);
  const ay = Math.abs(diry);
  const sq = eyeState.squish;
  const stretchX = Math.max(1 + sq * (0.25 * ax - 0.12 * ay), 0.6);
  const stretchY = Math.max(1 + sq * (0.25 * ay - 0.12 * ax), 0.6);

  // audio-driven motion for the whole face
  const kickP = Math.max(0, Math.min(audio.kick.p, 1.2));
  const chill = audio.chill;
  const xdW = Math.min(xd, 1); // how much the eyes are currently the > < chevrons
  const bang = audio.bang;
  const nodP = Math.max(-0.4, Math.min(audio.nod.p, 1.4));
  // while head-banging the head only moves on the kick (the usual bass-band bob
  // and beat lift would also follow vocals and melody, so they fade out)
  const groupBob =
    -(audio.bands[0] * 1.5 * (1 - 0.8 * chill) + kickP * 1.7) * audio.dance * sc * (1 - bang) + nodP * 6.5 * bang * sc;
  // the > < chevrons rock gently side to side (~1.3 Hz, a happy wiggle rather
  // than a shake); not tied to the beat
  const tt = performance.now() / 1000;
  const shakeX = (Math.sin(tt * 8.2) + 0.3 * Math.sin(tt * 4.6 + 1.3)) * (1.3 + 0.5 * audio.level) * xdW * sc;
  const swayX = Math.sin(audio.sway) * 2.6 * (1 - 0.35 * chill) * (1 - 0.8 * bang) * audio.dance * sc + shakeX;
  const tilt = Math.sin(audio.sway) * 0.11 * (1 - 0.6 * chill) * audio.dance * (1 - xdW) * (1 - bang); // head-nod lean
  const kickSquash = 1 + kickP * 0.16 * audio.dance * (1 - bang) - nodP * 0.16 * bang;
  const talkAmt = Math.max(-0.32, Math.min(audio.talkEnv - 0.32, 0.68)) * audio.talk * 1.8;
  const talkY = 1 + talkAmt * 0.8;
  const talkX = 1 - talkAmt * 0.24;

  targetCtx.save();
  targetCtx.translate(centerX + swayX, cy + groupBob);
  targetCtx.rotate(tilt);
  targetCtx.translate(-centerX, -cy);

  for (const side of [-1, 1]) {
    const ex = centerX + side * 13.5 * sc;
    let [gx, gy] = gazeFor(ex, cy);
    // spatial awareness: glance toward wherever the sound is panned
    gx += audio.pan * 0.6 * Math.min(1, audio.level * 2);

    // a wink is just this one eye morphing into the happy arch and back
    const winking = eyeState.winkSide === side;
    const eyeHappy = winking ? Math.max(happy, 1 - eyeState.openness) : happy;
    const p = eyeParams(eyeHappy, sleepy, wide, xd, bliss);
    const blink = winking ? 1 : Math.max(eyeState.openness, 0.06);

    targetCtx.save();
    // without pupils the whole eye does the looking
    targetCtx.translate(ex + gx * 3.2 * sc, cy + gy * 1.7 * sc);
    targetCtx.scale(sc * talkX, sc * talkY * kickSquash);
    // eyelids always close vertically, whatever else the eye is doing
    targetCtx.scale(1, blink);
    // the ^ ^ arch / sleepy lids stay undistorted: squash fades out as they form
    const calm = 1 - Math.min(Math.max(eyeHappy, sleepy, xd, bliss), 1);
    targetCtx.scale(1 + (stretchX - 1) * calm, 1 + (stretchY - 1) * calm);

    // the > < chevrons are the arch turned toward the middle (left eye >, right eye <)
    if (xd > 0.01) targetCtx.rotate(-side * (Math.PI / 2) * Math.min(xd, 1));
    eyePath(targetCtx, p, 1);
    const g = targetCtx.createRadialGradient(-p.rx * 0.3, -p.top * 0.35, 0, -p.rx * 0.3, -p.top * 0.35, p.rx * 2);
    g.addColorStop(0, "rgb(252, 252, 254)");
    g.addColorStop(1, "rgb(222, 226, 236)");
    targetCtx.fillStyle = g;
    targetCtx.fill();
    targetCtx.restore();
  }

  targetCtx.restore();
}

function paintEyes() {
  const w = canvas.width / dpr;
  const h = canvas.height / dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  // breathing: the eyes drift up and down a hair, on a slow cycle
  const t = performance.now();
  const cy = h / 2 + Math.sin((t / 3600) * Math.PI * 2) * 0.55;
  paintEyesAt(ctx, w / 2, cy);
}

function paintHubEyes() {
  const w = hubEyesCanvas.width / dpr;
  const h = hubEyesCanvas.height / dpr;
  hubEyesCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
  hubEyesCtx.clearRect(0, 0, w, h);

  const cx = w / 2;
  const cy = h / 2;

  paintEyesAt(hubEyesCtx, cx, cy, { scale: 1.3 });
}

// -- small shared helpers for the compact views --
const ICON = {
  prev: '<svg viewBox="0 0 24 24"><path d="M6 6h2v12H6zM20 6v12L9.5 12z"/></svg>',
  next: '<svg viewBox="0 0 24 24"><path d="M16 6h2v12h-2zM4 6l10.5 6L4 18z"/></svg>',
  play: '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>',
  pause: '<svg viewBox="0 0 24 24"><path d="M6 5h4v14H6zM14 5h4v14h-4z"/></svg>',
  bell: '<svg viewBox="0 0 24 24"><path d="M12 22a2.5 2.5 0 0 0 2.4-2h-4.8A2.5 2.5 0 0 0 12 22zm7-6V11a7 7 0 0 0-5-6.7V3a2 2 0 0 0-4 0v1.3A7 7 0 0 0 5 11v5l-2 2v1h18v-1z"/></svg>',
};


// ---- line icons used across the island: one stroke style, coloured by the text around them ----
const LI_PATHS = {
  gamepad: "M4.6 5.5h6.8a3 3 0 0 1 2.9 3.7l-.6 2.3a1.5 1.5 0 0 1-2.6.5L10 10.5H6l-1.1 1.5a1.5 1.5 0 0 1-2.6-.5l-.6-2.3a3 3 0 0 1 2.9-3.7zM5.2 7.3v2.6M3.9 8.6h2.6M10.6 7.8h.01M12.1 9.3h.01",
  briefcase: "M2.5 5.5h11v7.5h-11zM6 5.5V4a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5M2.5 9h11",
  lock: "M4 7.2h8v6.3H4zM5.6 7.2V5.2a2.4 2.4 0 0 1 4.8 0v2M8 9.6v1.6",
  unlock: "M4 7.2h8v6.3H4zM5.6 7.2V5.2a2.4 2.4 0 0 1 4.6-.9M8 9.6v1.6",
  shrink: "M6 3v3H3M10 3v3h3M6 13v-3H3M10 13v-3h3",
  term: "M2.5 3.5h11v9h-11zM5 6.5 7 8.2 5 9.9M8.5 10h2.5",
  search: "M7 3a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM10 10l3 3",
  phone: "M3.6 2.6h2.4l1.1 2.8-1.5 1a7.4 7.4 0 0 0 3.4 3.4l1-1.5 2.8 1.1v2.4a1.4 1.4 0 0 1-1.5 1.4A10.6 10.6 0 0 1 2.2 4.1a1.4 1.4 0 0 1 1.4-1.5zM10 2.4a3.6 3.6 0 0 1 3.6 3.6M10 4.7a1.4 1.4 0 0 1 1.3 1.3",
  moon: "M12.5 9.8A5 5 0 0 1 6.2 3.5a5 5 0 1 0 6.3 6.3z",
  chevron: "M4.5 6.5 8 10l3.5-3.5",
  download: "M8 2.5v7.6M4.8 7.3 8 10.5l3.2-3.2M3 13h10",
  eye: "M1.8 8S4 4 8 4s6.2 4 6.2 4-2.2 4-6.2 4S1.8 8 1.8 8zM8 6.4a1.6 1.6 0 1 0 0 3.2 1.6 1.6 0 0 0 0-3.2z",
  eyeoff: "M2 8s2.2-4 6-4c.9 0 1.7.2 2.4.5M14 8s-2.2 4-6 4c-.9 0-1.7-.2-2.4-.5M3 13 13 3",
  power: "M8 2v5.6M4.7 4.5a5 5 0 1 0 6.6 0",
  cursor: "M3.2 2.6 12.6 6.5 8.7 8.2 7 12.4z",
  follow: "M2.5 8h11M5 5.5 2.5 8 5 10.5M11 5.5 13.5 8 11 10.5",
  wave: "M3 6.2v3.6M6 3.6v8.8M9 5.6v4.8M12 4.2v7.6",
  timer: "M8 3.4a5 5 0 1 0 0 9.9 5 5 0 0 0 0-9.9zM8 5.8V8.4l1.7 1.1M6.6 1.6h2.8",
  hourglass: "M4.5 2.5h7M4.5 13.5h7M5 2.5c0 3 3 3.4 3 5.5s-3 2.5-3 5.5M11 2.5c0 3-3 3.4-3 5.5s3 2.5 3 5.5",
  image: "M2.5 3.5h11v9h-11zM2.5 10.6l3.2-3.2 3 3 2-2 2.8 2.8M10.6 6.3h.01",
  expand: "M3 6V3h3M13 6V3h-3M3 10v3h3M13 10v3h-3",
  contrast: "M8 2.5a5.5 5.5 0 1 0 0 11 5.5 5.5 0 0 0 0-11zM8 2.5v11",
  calendar: "M2.5 4.5h11v9h-11zM2.5 7.5h11M5.5 2.5v3M10.5 2.5v3",
  bell: "M4.5 11V7.5a3.5 3.5 0 0 1 7 0V11l1 1.2h-9zM6.7 13.6a1.4 1.4 0 0 0 2.6 0",
  link: "M6.8 9.2a2.6 2.6 0 0 0 3.7 0l2-2a2.6 2.6 0 0 0-3.7-3.7l-.9.9M9.2 6.8a2.6 2.6 0 0 0-3.7 0l-2 2a2.6 2.6 0 0 0 3.7 3.7l.9-.9",
  code: "M5.5 4.5 2 8l3.5 3.5M10.5 4.5 14 8l-3.5 3.5M9 3.5l-2 9",
  pen: "M10.5 2.8l2.7 2.7-7.3 7.3-3.2.5.5-3.2zM9 4.3l2.7 2.7",
  sun: "M8 5.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5zM8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M3.6 12.4l1.1-1.1M11.3 4.7l1.1-1.1",
  sparkle: "M8 2l1.4 3.6L13 7l-3.6 1.4L8 12l-1.4-3.6L3 7l3.6-1.4zM12.5 11.5v2.5M11.3 12.7h2.4",
  mail: "M2.5 4h11v8h-11zM2.5 4.6 8 9l5.5-4.4",
  music: "M6 12.2V3.8l7-1.3v8.4M6 12.2a1.7 1.7 0 1 1-3.4 0 1.7 1.7 0 0 1 3.4 0zM13 10.9a1.7 1.7 0 1 1-3.4 0 1.7 1.7 0 0 1 3.4 0z",
  globe: "M8 2a6 6 0 1 0 0 12A6 6 0 0 0 8 2zM2 8h12M8 2c2 1.7 2.8 3.7 2.8 6S10 12.3 8 14c-2-1.7-2.8-3.7-2.8-6S6 3.7 8 2z",
  dot: "M8 6.2a1.8 1.8 0 1 0 0 3.6 1.8 1.8 0 0 0 0-3.6z",
  plus: "M8 3.5v9M3.5 8h9",
  minus: "M3.5 8h9",
  x: "M4 4l8 8M12 4l-8 8",
  clock: "M8 2.5a5.5 5.5 0 1 0 0 11 5.5 5.5 0 0 0 0-11zM8 5v3.2l2.2 1.3",
  sliders: "M3 5h6.5M12 5h1M3 11h1.5M7 11h6M9.5 3.4v3.2M4.5 9.4v3.2",
  file: "M4 2.5h5l3 3v8H4zM9 2.5v3h3",
  check: "M3.5 8.5 6.5 11.5 12.5 4.5",
  pillshape: "M4.5 5.5h7a2.5 2.5 0 0 1 0 5h-7a2.5 2.5 0 0 1 0-5z",
};

function li(name) {
  return `<svg class="li" viewBox="0 0 16 16" aria-hidden="true"><path d="${LI_PATHS[name] || LI_PATHS.dot}"/></svg>`;
}

// an element that starts with an icon, then plain text (never HTML)
function withIcon(node, name, text) {
  node.innerHTML = li(name);
  node.append(document.createTextNode(` ${text}`));
  return node;
}

function setPlayIcon(btn, playing) {
  const want = playing ? "pause" : "play";
  if (btn.dataset.icon !== want) {
    btn.dataset.icon = want;
    btn.innerHTML = ICON[want];
  }
}

// long text scrolls back and forth instead of truncating
function setMarquee(box, text) {
  if (box.dataset.text === text) return;
  box.dataset.text = text;
  box.classList.remove("marquee", "run");
  box.replaceChildren(document.createTextNode(text));
  requestAnimationFrame(() => {
    const over = box.scrollWidth - box.clientWidth;
    if (over > 4) {
      const span = document.createElement("span");
      span.textContent = text;
      box.replaceChildren(span);
      span.style.setProperty("--dist", `${-(over + 8)}px`);
      box.classList.add("marquee", "run");
    }
  });
}

// pop a view's children in with a small stagger (see .enter in style.css)
function playEnter(container) {
  container.classList.remove("enter");
  Array.from(container.children).forEach((c, i) => c.style.setProperty("--i", i));
  void container.offsetWidth;
  container.classList.add("enter");
}

// Authoritative view name ("idle" | "media" | "game"), driven by Rust's
// view-tick -- Rust owns the resize/priority decision (media beats game),
// the frontend just reflects it rather than re-deriving it from has_session/
// has_game flags that could race the resize.
let currentView = "idle";

// -- Claude usage peek: bars sit at the old value, then fill to the new one
// once the island has landed (Rust decides when to show it) --
const usagePeekEl = document.getElementById("usage-peek");
let usagePeekData = null;
let usagePeekTimer = null;
listen("usage-peek", (e) => {
  usagePeekData = e.payload;
});

function playUsagePeek() {
  if (!usagePeekData) return;
  const { before, after } = usagePeekData;
  const keys = ["five_hour", "seven_day"];
  for (const k of keys) {
    const fill = document.getElementById(`peek-fill-${k}`);
    fill.classList.remove("animating");
    fill.style.width = `${before[k] ?? after[k] ?? 0}%`;
    document.getElementById(`peek-pct-${k}`).textContent =
      before[k] == null ? "" : `${Math.round(before[k])}%`;
  }
  clearTimeout(usagePeekTimer);
  usagePeekTimer = setTimeout(() => {
    for (const k of keys) {
      const fill = document.getElementById(`peek-fill-${k}`);
      fill.classList.add("animating");
      fill.style.width = `${after[k] ?? 0}%`;
      document.getElementById(`peek-pct-${k}`).textContent =
        after[k] == null ? "" : `${Math.round(after[k])}%`;
    }
  }, 500);
}


// -- audio visualizer: an aura that lives in the island itself instead of bars.
// Soft colour orbs drift through the pill, each fed by one frequency band;
// every beat sends a ripple out from the eyes; a voice
// gets a warm halo that pulses with the syllables. Drawn under all content. --
const vizCanvas = document.getElementById("viz");
const vizCtx = vizCanvas.getContext("2d");
const VIZ_SCALE = 0.7; // render below device resolution: it's all soft glow anyway
const viz = { rings: [], hue: 150, on: false, lastSyl: 0, lastBreath: 0 };

// the aura also bleeds outside the island into the window's margin (settings: Ambient, 0 = off)
const bleedCanvas = document.getElementById("viz-bleed");
const bleedCtx = bleedCanvas.getContext("2d");
let bleedLevel = 0.6; // 0..1

function sizeViz() {
  // (layout size: the pinned island may be drawn scaled down, the buffer must not follow)
  vizCanvas.width = Math.max(1, Math.round(pill.offsetWidth * VIZ_SCALE));
  vizCanvas.height = Math.max(1, Math.round(pill.offsetHeight * VIZ_SCALE));
  bleedCanvas.width = Math.max(1, Math.round(window.innerWidth * VIZ_SCALE));
  bleedCanvas.height = Math.max(1, Math.round(window.innerHeight * VIZ_SCALE));
}

function paintBleed() {
  const r = pill.getBoundingClientRect();
  const S = VIZ_SCALE;
  // reach follows the setting (it can never go past the margin the window has)
  // (the blur spreads it about twice as far again: half the margin is left for that,
  // or the glow would be cut off by the window's edge)
  const reach = Math.min(cursorPad * 0.5, 24) * bleedLevel;
  bleedCtx.setTransform(1, 0, 0, 1, 0, 0);
  bleedCtx.clearRect(0, 0, bleedCanvas.width, bleedCanvas.height);
  // the aura inside is faint (it sits under content): stack it a few times so the bleed reads
  // the light takes the island's own shape: clipped to its rounded outline, grown by the reach
  const x = (r.left - reach) * S;
  const y = (r.top - reach) * S;
  const w = (r.width + 2 * reach) * S;
  const h = (r.height + 2 * reach) * S;
  const radius = Math.min((parseFloat(getComputedStyle(pill).borderTopLeftRadius) || 0) + reach, r.height / 2 + reach) * S;
  bleedCtx.save();
  bleedCtx.beginPath();
  bleedCtx.roundRect(x, y, w, h, Math.min(radius, w / 2, h / 2));
  bleedCtx.clip();
  bleedCtx.globalCompositeOperation = "lighter";
  bleedCtx.globalAlpha = Math.min(1, bleedLevel * 1.25);
  for (let i = 0; i < 3; i++) bleedCtx.drawImage(vizCanvas, x, y, w, h);
  bleedCtx.restore();
}

function clearBleed() {
  bleedCtx.clearRect(0, 0, bleedCanvas.width, bleedCanvas.height);
  document.body.classList.remove("bleeding");
}
new ResizeObserver(sizeViz).observe(pill);
sizeViz();

// where the eyes sit inside the pill, in CSS px
function vizEyeCenter() {
  return [pill.offsetWidth / 2, currentView === "hub" ? 38 : pill.offsetHeight / 2];
}

// One ring per real audio onset -- no merging and no rate limit beyond the
// analyzer's ~90 ms onset spacing, so a fast run of hits gives a fast run of
// rings, while nothing is drawn that the audio did not actually do. (A cap
// keeps a pathological input from piling up rings.)
function pushRing(now, s, hue, pan) {
  viz.rings.push({ t: now, s, sv: s, hue, pan: pan || 0 });
  if (viz.rings.length > 12) viz.rings.shift();
}

function vizBeat(strength, voice, pan) {
  if (voice > 0.6) return; // speech gets the halo, not beat effects
  pushRing(performance.now(), strength, viz.hue + 20, pan);
}


// -- chill mode: soft circular gradient lights. A handful of round colour
// glows drift slowly around the island, overlapping and blending into each
// other, breathing gently with the loudness. --
function paintChillGradient(c, w, h, now, k, hub) {
  const T = now / 1000;
  const lvl = 0.6 + 0.4 * audio.level;
  const breathe = 0.85 + 0.15 * Math.sin(T * 0.8);
  const size = Math.max(w, h);
  c.globalCompositeOperation = "lighter"; // overlapping lights add up like real light

  const palette = [190, 250, 300, 340, 25];
  for (let i = 0; i < 5; i++) {
    const hue = (palette[i] + Math.sin(T * 0.11 + i) * 30 + T * 3) % 360;
    const x = w * (0.5 + 0.46 * Math.sin(T * (0.13 + 0.05 * i) + i * 1.9));
    const y = h * (0.5 + 0.46 * Math.sin(T * (0.17 + 0.04 * i) + i * 2.7));
    const r = size * (hub ? 0.34 : 0.44) * (0.85 + 0.15 * Math.sin(T * 0.4 + i * 1.3));
    const a = k * 0.42 * lvl * breathe;
    const g = c.createRadialGradient(x, y, 0, x, y, r);
    g.addColorStop(0, `hsla(${hue}, 80%, 62%, ${a})`);
    g.addColorStop(1, `hsla(${hue}, 80%, 62%, 0)`);
    c.fillStyle = g;
    c.fillRect(x - r, y - r, r * 2, r * 2);
  }
}

function paintViz(now, dt) {
  const c = vizCtx;
  const w = vizCanvas.width;
  const h = vizCanvas.height;
  const S = VIZ_SCALE;
  const live = audio.level > 0.03 || viz.rings.length > 0;
  if (!live) {
    if (viz.on) {
      c.clearRect(0, 0, w, h);
      viz.on = false;
      clearBleed();
    }
    return;
  }
  viz.on = true;
  c.setTransform(1, 0, 0, 1, 0, 0);
  c.clearRect(0, 0, w, h);
  c.globalCompositeOperation = "lighter";

  viz.hue = (viz.hue + dt * 7 * (1 - 0.75 * audio.chill)) % 360;
  const chill = audio.chill;
  const t = (now / 1000) * (1 - 0.55 * chill); // orbs drift slower
  // hue: toward a calm teal-blue as the music gets chill
  const dHue = ((200 + Math.sin(now / 4000) * 20 - viz.hue + 540) % 360) - 180;
  const baseHue = viz.hue + dHue * chill * 0.85;
  // the hub is mostly text -- keep the aura subtle there
  const gain = currentView === "hub" ? 0.5 : currentView === "idle" ? 1 : 0.7;
  const musicish = 0.35 + 0.65 * (1 - audio.voiceSm);
  const size = Math.max(w, h);

  // orbs: bass / low-mid / mid / high, each wandering on its own slow path
  const hues = [baseHue, baseHue + 45 * (1 - 0.5 * chill), baseHue + 115 * (1 - 0.6 * chill), baseHue + 195 * (1 - 0.7 * chill)];
  for (let i = 0; i < 4; i++) {
    const env = audio.bands[i];
    const px = w * (0.5 + 0.38 * Math.sin(t * (0.45 + 0.17 * i) + i * 1.7) + audio.pan * 0.12);
    const py =
      currentView === "hub"
        ? h * (0.1 + 0.16 * (0.5 + 0.5 * Math.sin(t * (0.6 + 0.13 * i) + i * 2.3)))
        : h * (0.5 + 0.3 * Math.sin(t * (0.6 + 0.13 * i) + i * 2.3));
    const r = size * (currentView === "hub" ? 0.3 : 0.24) * (0.55 + 0.75 * env);
    const a = (0.04 + 0.3 * env) * audio.level * gain * musicish * 1.4 * (1 - 0.75 * chill);
    const g = c.createRadialGradient(px, py, 0, px, py, r);
    g.addColorStop(0, `hsla(${hues[i]}, 90%, 62%, ${a})`);
    g.addColorStop(1, `hsla(${hues[i]}, 90%, 62%, 0)`);
    c.fillStyle = g;
    c.fillRect(0, 0, w, h);
  }

  const [ecx, ecy] = vizEyeCenter();
  const cx = ecx * S;
  const cy = ecy * S;

  if (chill > 0.04) {
    const showGain = currentView === "hub" ? 0.6 : currentView === "idle" ? 1 : 0.8;
    paintChillGradient(c, w, h, now, chill * showGain, currentView === "hub");
  }

  // voice: a warm halo breathing with the syllables, plus a soft ripple per syllable
  if (audio.talk > 0.03) {
    const env = audio.talkEnv;
    const r = size * (0.16 + 0.3 * env);
    const g = c.createRadialGradient(cx, cy, 0, cx, cy, r);
    g.addColorStop(0, `hsla(38, 95%, 70%, ${0.3 * audio.talk * gain * (0.4 + env)})`);
    g.addColorStop(1, "hsla(38, 95%, 70%, 0)");
    c.fillStyle = g;
    c.fillRect(0, 0, w, h);
    if (env > 0.5 && t - viz.lastSyl > 0.28) {
      viz.lastSyl = t;
      pushRing(now, Math.min(1, 0.2 + env * 0.8), 38, audio.pan);
    }
  }

  // ripples: very thin circular waves expanding from the middle between the eyes. Sound panned to a
  // side shows as a ring that is solid on that side and translucent on the other.
  c.lineWidth = 0.7; // hairline, always -- only the fade changes
  c.lineCap = "round";
  for (let i = viz.rings.length - 1; i >= 0; i--) {
    const rg = viz.rings[i];
    // rAF timestamps can trail performance.now() by a hair: never let age go negative
    const age = Math.max(0, (now - rg.t) / 900);
    if (age >= 1) {
      viz.rings.splice(i, 1);
      continue;
    }
    const ease = 1 - Math.pow(1 - age, 2.2);
    // merged hits raise the target strength; the drawn strength eases toward it
    rg.sv += (rg.s - rg.sv) * Math.min(1, dt * 9);
    const side = Math.max(-1, Math.min(1, rg.pan || 0));
    const dirness = Math.max(0, Math.min((Math.abs(side) - 0.12) / 0.6, 1)); // 0 centered .. 1 hard-panned
    // how far it travels follows how strong the sound was: soft = a short
    // ripple, a hard hit or loud voice = a wave that crosses the whole island
    const reach = 0.12 + 0.62 * rg.sv;
    const radius = Math.max(0, ease * size * reach);
    // always a full circle from the middle; the direction shows as the translucency on the far side
    const ox = cx;
    // near-opaque at birth, fading out slowly so the hairline stays easy to see
    const alpha = Math.min(1, (0.6 + 0.4 * rg.sv) * Math.pow(1 - age, 0.7)) * (currentView === "hub" ? 0.85 : 1);
    if (dirness > 0.02 && radius > 1) {
      // sound from one side: the ring is solid on the side it comes from and turns translucent
      // across to the other, more so the harder it is panned
      const far = alpha * (1 - 0.85 * dirness);
      const g = c.createLinearGradient(ox - side * radius, cy, ox + side * radius, cy);
      g.addColorStop(0, `hsla(${rg.hue}, 95%, 76%, ${far})`);
      g.addColorStop(0.5, `hsla(${rg.hue}, 95%, 76%, ${(far + alpha) / 2})`);
      g.addColorStop(1, `hsla(${rg.hue}, 95%, 76%, ${alpha})`);
      c.strokeStyle = g;
    } else {
      c.strokeStyle = `hsla(${rg.hue}, 95%, 76%, ${alpha})`;
    }
    c.beginPath();
    c.arc(ox, cy, radius, 0, Math.PI * 2);
    c.stroke();
  }
  c.globalCompositeOperation = "source-over";
  const bleeding = bleedLevel > 0 && cursorPad > 0;
  document.body.classList.toggle("bleeding", bleeding);
  if (bleeding) paintBleed();
}

let lastFrameT = performance.now();
function frame(now) {
  // one bad frame must never kill the loop (the eyes would freeze/vanish for good)
  try {
    const dt = Math.min(Math.max((now - lastFrameT) / 1000, 0), 0.05);
    lastFrameT = now;
    updateCall(now);
    updateAudio(now, dt);
    paintViz(now, dt);
    if (currentView === "idle") {
      updateMood(now);
      tickBlink(now);
      paintEyes();
    } else if (currentView === "hub") {
      updateMood(now);
      tickBlink(now);
      paintHubEyes();
    }
  } catch (err) {
    console.error(err);
  }
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);

listen("view-tick", (event) => {
  currentView = event.payload;
  mediaEl.classList.toggle("hidden", currentView !== "media");
  gameEl.classList.toggle("hidden", currentView !== "game");
  workEl.classList.toggle("hidden", currentView !== "work");
  notifEl.classList.toggle("hidden", currentView !== "notification");
  briefEl.classList.toggle("hidden", currentView !== "brief");
  hubEl.classList.toggle("hidden", currentView !== "hub");
  pill.dataset.view = currentView;
  // the brief-show views arrive with a glow; restart it each time one takes over
  document.body.dataset.view = currentView;
  document.body.classList.remove("glow");
  if (currentView !== "idle" && currentView !== "hub") {
    void document.body.offsetWidth;
    document.body.classList.add("glow");
  }
  updateBg();
  const enterTarget = { media: mediaEl, game: gameEl, work: workEl, notification: notifEl, brief: briefEl, usage_peek: usagePeekEl }[currentView];
  if (enterTarget) playEnter(enterTarget);
  usagePeekEl.classList.toggle("hidden", currentView !== "usage_peek");
  if (currentView === "usage_peek") playUsagePeek();
  if (currentView !== "idle") {
    // the canvas stops repainting once a view takes over -- clear its last
    // frame so stale eye pixels don't linger visible through any gaps
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }
  if (currentView === "hub") {
    // hub-eyes was `display:none` (0x0) at the one point page-load sizing
    // ran, and toggling a CSS class doesn't fire a `resize` event to catch
    // it later -- size it explicitly now that it's actually visible
    sizeCanvasToElement(hubEyesCanvas);
    mood.happyUntil = performance.now() + 1600; // happy to see you
    // left-click opens the info pane; the tray's Settings item asks for settings
    setHubPane(pendingSettingsPane ? "settings" : "info");
    pendingSettingsPane = false;
    updateClock();
    playHubEntrance(document.getElementById("hub-head"));
    loadSettingsIntoForm();
    loadNotificationHistory();
    loadUsage();
    loadCalendar();
    refreshActivity();
    pollSysStats();
  }
});

// -- cursor tracking + squash/stretch, driven by Rust's global cursor poll --
// -- the island being "called": a glow builds at the top edge while the cursor dwells --
const callGlow = document.getElementById("call-glow");
let cursorPad = 0;
let callTarget = 0;
let callSmooth = 0;
let callLast = 0;

function updateCall(nowMs) {
  // follows the charge exactly while it builds; the end is very quick (about 0.14 s to gone)
  const dt = Math.min(Math.max((nowMs - callLast) / 1000, 0), 0.05);
  callLast = nowMs;
  callSmooth = callTarget > callSmooth ? callTarget : callTarget + (callSmooth - callTarget) * Math.exp(-dt / 0.025);
  if (callTarget === 0 && callSmooth < 0.004) callSmooth = 0;
  document.body.classList.toggle("calling", callSmooth > 0);
  // the pill stays out of sight while the glow charges; it appears with the drain
  document.body.classList.toggle("call-hide-pill", callTarget > 0);
  if (callSmooth > 0) callGlow.style.setProperty("--charge", callSmooth.toFixed(3));
  // the end fades as well as shrinks
  callGlow.style.setProperty("--fade", callTarget < callSmooth ? callSmooth.toFixed(3) : "1");
}

listen("cursor-tick", (event) => {
  // ease-in curve, as picked in the Edge Glow Lab
  const rawCharge = event.payload.charge || 0;
  callTarget = rawCharge * rawCharge;
  // the window is enlarged while charging and while the island lands: keep the pill at its own size
  // and the glow on the screen's top edge
  // pinned and left alone: drawn smaller (CSS scales the island, see body.shrunk)
  const shrunk = !!event.payload.shrunk;
  if (shrunk !== document.body.classList.contains("shrunk")) {
    document.documentElement.style.setProperty("--shrink", String((event.payload.shrink || 75) / 100));
    document.body.classList.toggle("shrunk", shrunk);
  }
  const big = !!event.payload.big;
  const pad = event.payload.pad || 0;
  if (pad !== cursorPad) {
    cursorPad = pad;
    document.documentElement.style.setProperty("--pad", `${pad}px`);
  }
  if (big !== document.body.classList.contains("big")) {
    document.body.classList.toggle("big", big);
    if (!big) pill.classList.remove("landing");
  }
  if (big) {
    const rs = document.documentElement.style;
    rs.setProperty("--edge", event.payload.edge.toFixed(1));
    rs.setProperty("--pill-top", event.payload.pill_top.toFixed(1));
    rs.setProperty("--pill-w", event.payload.pill_w);
    rs.setProperty("--pill-h", event.payload.pill_h);
  }
  const { local_x, local_y } = event.payload;
  const dx = local_x - eyeState.lastCursor.x;
  const dy = local_y - eyeState.lastCursor.y;
  eyeState.lastCursor = { x: local_x, y: local_y };

  const speed = Math.hypot(dx, dy);
  const nowMs = performance.now();
  if (speed > 2) {
    if (nowMs - mood.lastMoveAt > 25000) mood.wideUntil = nowMs + 500; // startled awake
    mood.lastMoveAt = nowMs;
  }
  if (islandShown !== !!event.payload.shown) {
    islandShown = !!event.payload.shown;
    pill.classList.toggle("hiding", !islandShown);
    document.body.classList.toggle("pill-hidden", !islandShown);
    if (islandShown && document.body.classList.contains("big")) {
      // the island lands inside the enlarged window: animate it in place
      pill.classList.remove("landing");
      void pill.offsetWidth;
      pill.classList.add("landing");
    }
    updateBg();
  }
  mood.hover =
    !!event.payload.shown &&
    local_x >= cursorPad && local_x <= window.innerWidth - cursorPad &&
    local_y >= cursorPad && local_y <= window.innerHeight - cursorPad;
  const targetSquish = Math.min(speed / 140.0, 1.0);
  if (targetSquish > eyeState.squish) {
    eyeState.squish = targetSquish;
    if (speed > 0.5) {
      eyeState.squishDir = [dx / speed, dy / speed];
    }
  } else {
    eyeState.squish *= 0.78;
  }

  eyeState.smoothedCursor.x += (local_x - eyeState.smoothedCursor.x) * 0.35;
  eyeState.smoothedCursor.y += (local_y - eyeState.smoothedCursor.y) * 0.35;
});

// -- drag to move (native OS move, not a JS-computed one -- cheaper and
// matches exactly how the window manager drags any other window) --
// settings live behind the tray menu's "Settings" item so they aren't always
// on screen (Rust emits "open-settings", then opens the hub if it was closed).
// Left-click opens the hub on the info pane.
let pendingSettingsPane = false;
const paneSettings = document.getElementById("pane-settings");
const paneInfo = document.getElementById("pane-info");
const paneCalendar = document.getElementById("pane-calendar");
const hubBody = document.getElementById("hub-body");

function setHubPane(pane) {
  paneSettings.classList.toggle("hidden", pane !== "settings");
  paneInfo.classList.toggle("hidden", pane !== "info");
  paneCalendar.classList.toggle("hidden", pane !== "calendar");
  hubBody.scrollTop = 0;
  scheduleHubHeight();
  playHubEntrance(pane === "settings" ? paneSettings : pane === "calendar" ? paneCalendar : paneInfo);
}

// -- adaptive hub height: the expanded island is only as tall as what it holds
// (up to Rust's max). Watch the panes and the header; whenever the visible
// pane's content height changes, tell Rust the height it should spring to. --
const hubHeadEl = document.getElementById("hub-head");
let hubHeightSent = 0;
let hubHeightRaf = 0;
function reportHubHeight() {
  hubHeightRaf = 0;
  if (currentView !== "hub") return;
  const pane = [paneSettings, paneInfo, paneCalendar].find((p) => !p.classList.contains("hidden"));
  if (!pane) return;
  // bottom edge of the last visible card. offsetTop/offsetHeight are layout
  // values (getBoundingClientRect would include the entrance animation's
  // transforms, and measure a pane mid-pop-in too tall)
  let bottom = 0;
  const visit = (el) => {
    for (const c of el.children) {
      if (getComputedStyle(c).display === "contents") visit(c);
      else if (c.offsetHeight > 0) bottom = Math.max(bottom, c.offsetTop + c.offsetHeight);
    }
  };
  visit(pane);
  const pb = parseFloat(getComputedStyle(hubBody).paddingBottom);
  const h = Math.ceil(hubHeadEl.offsetHeight + bottom + pb);
  if (Math.abs(h - hubHeightSent) < 2) return;
  hubHeightSent = h;
  invoke("set_hub_height", { h });
}
function scheduleHubHeight() {
  if (!hubHeightRaf) hubHeightRaf = requestAnimationFrame(reportHubHeight);
}
const hubHeightObserver = new ResizeObserver(scheduleHubHeight);
for (const el of [paneSettings, paneInfo, paneCalendar, hubHeadEl]) hubHeightObserver.observe(el);

// stagger the pane's children springing up (CSS `rise` keyframes, --i = order)
function playHubEntrance(container) {
  container.classList.remove("enter");
  const items = [];
  for (const c of container.children) {
    if (c.classList.contains("flat")) items.push(...c.children);
    else items.push(c);
  }
  items.forEach((c, i) => c.style.setProperty("--i", i));
  void container.offsetWidth; // restart the animation
  container.classList.add("enter");
  // once played, clear it: cards inserted later (data arriving, list rebuilds) shouldn't re-pop
  clearTimeout(container._enterTimer);
  container._enterTimer = setTimeout(() => container.classList.remove("enter"), 1100);
}

// header clock (DOM, not canvas -- crisper and styleable)
const hubTimeEl = document.getElementById("hub-time");
const hubDateEl = document.getElementById("hub-date");
function updateClock() {
  const now = new Date();
  const t = now.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (hubTimeEl.textContent !== t) hubTimeEl.textContent = t;
  hubDateEl.textContent = now.toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
}
setInterval(() => {
  if (currentView === "hub") updateClock();
}, 1000);

// cursor spotlight: every .spot card gets the cursor position (relative to
// itself) as --mx/--my, which its CSS radial-gradient follows like a flashlight
const hubRoot = document.getElementById("hub");
let spotRaf = 0;
let spotEvent = null;
function updateSpots() {
  spotRaf = 0;
  if (!spotEvent) return;
  for (const el of hubRoot.querySelectorAll(".spot")) {
    const r = el.getBoundingClientRect();
    el.style.setProperty("--mx", `${spotEvent.clientX - r.left}px`);
    el.style.setProperty("--my", `${spotEvent.clientY - r.top}px`);
  }
}
hubRoot.addEventListener("mousemove", (e) => {
  spotEvent = e;
  hubRoot.classList.add("spot-on");
  if (!spotRaf) spotRaf = requestAnimationFrame(updateSpots);
});
hubRoot.addEventListener("mouseleave", () => {
  // keep --mx/--my where they were: the light fades out in place (--spot-a)
  spotEvent = null;
  hubRoot.classList.remove("spot-on");
});

listen("open-settings", () => {
  if (currentView === "hub") setHubPane("settings");
  else pendingSettingsPane = true;
});

// no native webview context menu on the island
// right-click pins the island in place (no hiding, no following the cursor to
// other monitors, hub stays open on click-outside) until right-clicked again.
// Feedback is a material-style ink ripple that floods the island from the click.
function inkRipple(x, y) {
  const r = pill.getBoundingClientRect();
  // radius that reaches the farthest corner from the click
  // (x1.3 so the soft edge of the gradient is still past the corner)
  const reach = 1.3 * Math.hypot(Math.max(x, r.width - x), Math.max(y, r.height - y));
  const dot = document.createElement("div");
  dot.className = "ink";
  dot.style.left = `${x - reach}px`;
  dot.style.top = `${y - reach}px`;
  dot.style.width = dot.style.height = `${reach * 2}px`;
  pill.append(dot);
  dot.addEventListener("animationend", (e) => {
    if (e.animationName === "ink-fade") dot.remove(); // the last of its two animations
  });
}

pill.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const r = pill.getBoundingClientRect();
  inkRipple(e.clientX - r.left, e.clientY - r.top);
  invoke("toggle_pin");
});
const lockBadge = document.getElementById("lock-badge");
listen("pin-tick", (event) => {
  mood.happyUntil = performance.now() + 900; // a little happy blip either way
  // a lock (pinned) or an open lock (unpinned) pops up at the centre of the island
  const pinned = !!event.payload;
  lockBadge.innerHTML = li(pinned ? "lock" : "unlock");
  lockBadge.classList.toggle("unlocked", !pinned);
  lockBadge.classList.remove("show");
  void lockBadge.offsetWidth;
  lockBadge.classList.add("show");
});

pill.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  mood.wideUntil = performance.now() + 320; // boop!
  invoke("drag_start");
});
// No mouseup/drag_end handler: `start_dragging()`'s native OS move-loop can
// swallow the DOM mouseup, so end-of-drag is detected Rust-side instead by
// polling the mouse button's real state -- see spawn_edge_poll in lib.rs.
// Hover (for pausing the idle-hide timer) is computed on the Rust side from
// the global cursor vs. the window's own rect -- see IslandState's doc
// comment in lib.rs for why webview mouseenter/leave aren't used for that.

// -- media pill: driven by media-tick events from the Rust SMTC poll thread --
const mediaEl = document.getElementById("media");
const mediaArt = document.getElementById("media-art");
const mediaTitle = document.getElementById("media-title");
const mediaArtist = document.getElementById("media-artist");
const mediaPlay = document.getElementById("media-play");
const mediaPrev = document.getElementById("media-prev");
const mediaNext = document.getElementById("media-next");

mediaPrev.innerHTML = ICON.prev;
mediaNext.innerHTML = ICON.next;
const mediaProgress = document.getElementById("media-progress");
const mediaProgressFill = mediaProgress.firstElementChild;

listen("media-tick", (event) => {
  const m = event.payload;
  // (also when the session is gone: that is what hides the hub's media card)
  applyHubMedia(m);
  if (!m.has_session) return;

  setMarquee(mediaTitle, m.title || "");
  mediaArtist.textContent = m.artist || "";
  if (m.art) mediaArt.src = m.art;
  mediaArt.classList.toggle("hidden", !m.art);
  setPlayIcon(mediaPlay, m.playing);
  mediaPrev.classList.toggle("hidden", !m.can_previous);
  mediaNext.classList.toggle("hidden", !m.can_next);
  // live streams report no duration -- no progress line then
  const hasProgress = m.duration > 0;
  mediaProgress.classList.toggle("hidden", !hasProgress);
  if (hasProgress) {
    mediaProgressFill.style.width = `${Math.min(100, (m.position / m.duration) * 100)}%`;
  }
});

for (const btn of [mediaPlay, mediaPrev, mediaNext]) {
  // stop the pill's own mousedown handler from starting a native window
  // drag out from under the click
  btn.addEventListener("mousedown", (e) => e.stopPropagation());
}
mediaPlay.addEventListener("click", () => invoke("media_play_pause"));
mediaPrev.addEventListener("click", () => invoke("media_previous"));
mediaNext.addEventListener("click", () => invoke("media_next"));

// -- game pill: driven by game-tick events from the Rust foreground-window poll --
const gameEl = document.getElementById("game");
const gameName = document.getElementById("game-name");
const gameMore = document.getElementById("game-more");
const gameIconImg = document.getElementById("game-icon-img");
const gameIconEmoji = document.getElementById("game-icon");

// the pill also shows how the game runs: FPS, then GPU / CPU / memory rings (instead of a dot)
let gamePid = 0;
let pillGauges = null;
const gameFpsEl = document.getElementById("game-fps");

async function pollPillGame() {
  if (currentView !== "game" || !gamePid) return;
  const [s] = await invoke("get_game_stats", { pids: [gamePid] });
  if (!pillGauges) {
    pillGauges = { gpu: statRing("#8ADB6E"), cpu: statRing("#5AC8E6"), ram: statRing("#B47EE6") };
    document.getElementById("game-rings").append(pillGauges.gpu.el, pillGauges.cpu.el, pillGauges.ram.el);
  }
  const hasFps = !!(s && s.fps != null);
  gameFpsEl.hidden = !hasFps;
  if (hasFps) gameFpsEl.firstElementChild.textContent = String(Math.round(s.fps));
  pillGauges.gpu.set(s ? s.gpu_pct : null, s && s.gpu_pct != null ? `GPU \u00b7 ${Math.round(s.gpu_pct)}%` : "");
  pillGauges.cpu.set(s ? s.cpu_pct : null, s ? `CPU \u00b7 ${Math.round(s.cpu_pct)}%` : "");
  pillGauges.ram.set(s ? s.ram_pct : null, s ? `RAM \u00b7 ${s.ram_gb.toFixed(1)} GB` : "");
}
setInterval(pollPillGame, 1000);

listen("game-tick", (event) => {
  const g = event.payload;
  gamePid = g.has_game ? g.pid || 0 : 0;
  if (!g.has_game) return;
  gameName.textContent = g.name || "";
  gameMore.textContent = g.count > 1 ? `+${g.count - 1}` : "";
  gameMore.classList.toggle("hidden", !(g.count > 1));
  if (g.icon) {
    gameIconImg.src = g.icon;
    gameIconImg.classList.remove("hidden");
    gameIconEmoji.classList.add("hidden");
  } else {
    gameIconImg.classList.add("hidden");
    gameIconEmoji.classList.remove("hidden");
  }
});

// -- work pill: driven by work-tick events from the Rust foreground-category poll --
const WORK_GLYPH = {
  coding: "code",
  writing: "pen",
  design: "sparkle",
  communication: "mail",
  media: "music",
  gaming: "gamepad",
  browsing: "globe",
  other: "dot",
};

const workEl = document.getElementById("work");
const workIcon = document.getElementById("work-icon");
const workLabel = document.getElementById("work-label");
const workApp = document.getElementById("work-app");
const workTime = document.getElementById("work-time");

listen("work-tick", (event) => {
  const w = event.payload;
  if (!w.has_session) return;
  workIcon.innerHTML = li(WORK_GLYPH[w.category] || "dot");
  workLabel.textContent = w.label || "";
  workApp.textContent = w.app_name || "";
  workTime.textContent = formatDuration(w.started_at_secs);
});

// -- notification banner: driven by notification-tick, and (for now) a
// pushNotification() global any future subsystem can call to show one --
const notifEl = document.getElementById("notification");
const notifTitle = document.getElementById("notif-title");
const notifBody = document.getElementById("notif-body");
const notifTimer = document.getElementById("notif-timer");
document.getElementById("notif-icon").innerHTML = ICON.bell;

let notifAction = null; // what clicking the current banner does (e.g. "viber")
let notifId = 0; // its entry in the scrollback
listen("notification-tick", (event) => {
  const n = event.payload;
  if (!n) return; // dismissed -- view-tick already hid the banner
  if (n.brief) {
    showBrief(n);
    return;
  }
  notifAction = n.action || null;
  notifId = n.id;
  notifTitle.textContent = n.title || "";
  notifBody.textContent = n.body || "";
  // restart the draining timer bar for this banner
  notifTimer.classList.remove("run");
  void notifTimer.offsetWidth;
  notifTimer.classList.add("run");
});

// clicking a banner dismisses it (and its card in the hub), running its action if it has one,
// instead of toggling the hub (its mousedown must not reach #pill's drag handler)
notifEl.addEventListener("mousedown", (e) => e.stopPropagation());
notifEl.addEventListener("click", () => invoke("click_notification", { id: notifId, action: notifAction }));

// -- a Claude Code session that needs you or finished: a compact pill from the same queue --
const briefEl = document.getElementById("brief");
const briefTile = document.getElementById("brief-tile");
const briefTitle = document.getElementById("brief-title");
const briefRight = document.getElementById("brief-right");
let briefCurrent = null;
function showBrief(n) {
  const b = n.brief;
  briefCurrent = b;
  briefTitle.textContent = n.title;
  briefTile.style.setProperty("--c", LLM_COLOR.claude);
  briefTile.innerHTML = `${LLM_LETTER.claude}<svg class="cr" viewBox="0 0 40 40"><circle class="rb" cx="20" cy="20" r="18"/><circle class="rf" cx="20" cy="20" r="18"/></svg>`;
  const rf = briefTile.querySelector(".rf");
  rf.style.strokeDasharray = String(LLM_RING_C);
  rf.style.strokeDashoffset = String(LLM_RING_C * (1 - b.ctx));
  rf.style.stroke = llmCtxColor(b.ctx);
  const host = el("span", "llm-host");
  host.innerHTML = li(b.host_icon);
  briefRight.replaceChildren(host, llmStateIcon(b.state));
  document.body.dataset.brief = b.state;
  // already a brief on screen (the queue moved on): the glow and the entrance start again
  if (currentView === "brief") {
    document.body.classList.remove("glow");
    void document.body.offsetWidth;
    document.body.classList.add("glow");
    playEnter(briefEl);
  }
}
briefEl.addEventListener("mousedown", (e) => e.stopPropagation());
briefEl.addEventListener("click", () => {
  if (!briefCurrent) return;
  invoke("focus_source", { exePath: briefCurrent.host_exe, titleHint: briefCurrent.project });
  if (briefCurrent.state === "finished") invoke("llm_dismiss", { id: briefCurrent.id });
  invoke("open_notification_action", { action: "brief" }); // ends the pill
});

window.pushNotification = (title, body) => invoke("push_notification", { title, body });

// -- notification history: the hub's scrollback of past banners --
const notifHistoryEl = document.getElementById("notif-history");

function renderNotificationHistory(entries) {
  notifHistoryEl.innerHTML = "";
  if (!entries || entries.length === 0) {
    updateInfoEmpty();
    return;
  }
  for (const entry of entries) {
    const item = document.createElement("div");
    item.className = "notif-history-item spot";
    // dismiss: the card slides off and folds away, then Rust forgets it
    const dismissItem = () => {
      item.style.height = `${item.offsetHeight}px`;
      void item.offsetWidth; // commit the explicit height so it can transition to 0
      item.classList.add("leaving");
      setTimeout(() => invoke("dismiss_notification", { id: entry.id }), 280);
    };
    // clicking a card clears it (the x does the same without going anywhere); one with an
    // action also jumps to its source app
    item.classList.add("clickable");
    item.addEventListener("mousedown", (e) => e.stopPropagation());
    item.addEventListener("click", () => {
      if (entry.action) invoke("open_notification_action", { action: entry.action });
      dismissItem();
    });

    const dismiss = document.createElement("button");
    dismiss.className = "notif-x";
    dismiss.setAttribute("aria-label", "Dismiss");
    dismiss.innerHTML =
      '<svg viewBox="0 0 12 12"><path d="M3 3l6 6M9 3l-6 6" /></svg>';
    dismiss.addEventListener("mousedown", (e) => e.stopPropagation());
    dismiss.addEventListener("click", (e) => {
      e.stopPropagation(); // dismiss only, don't also open the source
      dismissItem();
    });
    item.appendChild(dismiss);

    const title = document.createElement("div");
    title.className = "notif-history-title";
    title.textContent = entry.title || "";
    item.appendChild(title);

    if (entry.body) {
      const body = document.createElement("div");
      body.className = "notif-history-body";
      body.textContent = entry.body;
      item.appendChild(body);
    }

    const time = document.createElement("div");
    time.className = "notif-history-time";
    time.textContent = new Date(entry.time_ms).toLocaleString([], {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
    item.appendChild(time);

    notifHistoryEl.appendChild(item);
  }
  updateInfoEmpty();
}

async function loadNotificationHistory() {
  renderNotificationHistory(await invoke("get_notification_history"));
}

listen("notification-history-tick", (event) => {
  // only bother re-rendering if the hub is actually open to see it
  if (currentView === "hub") {
    renderNotificationHistory(event.payload);
  }
});

// -- hub: the pill expanded into a panel, toggled by clicking the pill
// (see drag_end in lib.rs -- a click is just a drag that went nowhere).
// Its eyes + clock are canvas-painted every frame in paintHubEyes() above,
// same as the idle pill's, so they stay live (blink/gaze-track) while open.
const hubEl = document.getElementById("hub");


// -- icons on the static markup: settings sections and rows, clear buttons, the game pill --
(function decorate() {
  const ROW = {
    "set-game-detection": "gamepad",
    "set-work-detection": "briefcase",
    "set-download-detection": "download",
    "set-llm-detection": "sparkle",
    "set-llm-brief": "phone",
    "set-page-preview": "eye",
    "set-start-with-windows": "power",
    "set-show-at-cursor": "cursor",
    "set-cursor-follow": "follow",
    "set-react-to-audio": "wave",
    "set-audio-bleed": "sun",
    "set-edge-dwell": "timer",
    "set-idle-hide-delay": "eyeoff",
    "set-peek-duration": "hourglass",
    "set-pin-shrink": "shrink",
    "set-compact-width": "follow",
    "set-glow": "sparkle",
    "bg-pick-compact": "image",
    "bg-pick-hub": "expand",
    "set-bg-dim": "contrast",
    "set-calendar-lead": "bell",
  };
  for (const [id, name] of Object.entries(ROW)) {
    const label = document.getElementById(id)?.closest(".hub-row")?.querySelector(":scope > span");
    if (label) {
      label.classList.add("hub-row-label");
      label.insertAdjacentHTML("afterbegin", li(name));
    }
  }
  const SECTION = { Behavior: "sliders", Island: "pillshape", Background: "image", Calendar: "calendar" };
  for (const t of document.querySelectorAll(".hub-section-title")) {
    const name = SECTION[t.textContent.trim()];
    if (name) {
      t.classList.add("with-icon");
      t.insertAdjacentHTML("afterbegin", li(name));
    }
  }

  // the short labels are explained by hovering them
  const HINT = {
    "set-game-detection": "Detect running games",
    "set-work-detection": "Detect what you are working on",
    "set-download-detection": "Show active downloads",
    "set-llm-detection": "Show the Claude Code sessions that are running",
    "set-llm-brief": "Drop the island down when a Claude Code session needs you or finishes",
    "set-page-preview": "Read the text of the page in front of you",
    "set-start-with-windows": "Start with Windows",
    "set-show-at-cursor": "Appear under the cursor",
    "set-cursor-follow": "Follow the cursor along the top edge",
    "set-react-to-audio": "Eyes react to sound",
    "set-audio-bleed": "Sound light bleeding outside the island: strength and reach (0 = off)",
    "set-edge-dwell": "How long the cursor rests at the edge before it appears",
    "set-idle-hide-delay": "How long before it hides again",
    "set-peek-duration": "How long a status stays up",
    "set-pin-shrink": "How small the island gets while pinned and left alone (100% = never)",
    "set-compact-width": "Width of the collapsed island",
    "set-glow": "Glow around a status: strength and reach",
    "bg-pick-compact": "Background of the island",
    "bg-pick-hub": "Background of the expanded island",
    "set-bg-dim": "How dark the background is",
    "set-calendar-url": "Paste the secret .ics address of a Google or Outlook calendar",
    "set-calendar-lead": "Minutes of warning before an event",
  };
  for (const [id, tip] of Object.entries(HINT)) {
    const row = document.getElementById(id)?.closest(".hub-row");
    if (row) row.title = tip;
  }
  document.querySelector(".field-icon").innerHTML = li("link");
  document.getElementById("set-calendar-clear").innerHTML = li("x");
  for (const b of document.querySelectorAll(".hub-btn-x")) b.innerHTML = li("x");
  const gameIcon = document.getElementById("game-icon");
  if (gameIcon) gameIcon.innerHTML = li("gamepad");
})();

// -- hub settings: loaded fresh each time the hub opens, saved back to
// Rust (which persists to disk and live-updates the detection threads)
// on every change --
const setGameDetection = document.getElementById("set-game-detection");
const setWorkDetection = document.getElementById("set-work-detection");
const setDownloadDetection = document.getElementById("set-download-detection");
const setLlmDetection = document.getElementById("set-llm-detection");
const setLlmBrief = document.getElementById("set-llm-brief");
const setPagePreview = document.getElementById("set-page-preview");
const setStartWithWindows = document.getElementById("set-start-with-windows");
const setIdleHideDelay = document.getElementById("set-idle-hide-delay");
const setIdleHideDelayLabel = document.getElementById("set-idle-hide-delay-label");

const setShowAtCursor = document.getElementById("set-show-at-cursor");
const setReactToAudio = document.getElementById("set-react-to-audio");
const setCursorFollow = document.getElementById("set-cursor-follow");
const setPeekDuration = document.getElementById("set-peek-duration");
const setPeekDurationLabel = document.getElementById("set-peek-duration-label");
const setCompactWidth = document.getElementById("set-compact-width");
const setCompactWidthLabel = document.getElementById("set-compact-width-label");
const setPinShrink = document.getElementById("set-pin-shrink");
const setPinShrinkLabel = document.getElementById("set-pin-shrink-label");
const pinShrinkText = (v) => (Number(v) >= 100 ? "off" : `${v}%`);
const setEdgeDwell = document.getElementById("set-edge-dwell");
const setEdgeDwellLabel = document.getElementById("set-edge-dwell-label");
const edgeDwellText = (ms) => (Number(ms) === 0 ? "off" : `${ms} ms`);
const setCalendarUrl = document.getElementById("set-calendar-url");
const setCalendarLead = document.getElementById("set-calendar-lead");
const setCalendarLeadLabel = document.getElementById("set-calendar-lead-label");
const setAudioBleed = document.getElementById("set-audio-bleed");
const setAudioBleedLabel = document.getElementById("set-audio-bleed-label");
const setGlow = document.getElementById("set-glow");
const setGlowLabel = document.getElementById("set-glow-label");
const setBgDim = document.getElementById("set-bg-dim");
const setBgDimLabel = document.getElementById("set-bg-dim-label");

let currentSettings = null;

async function loadSettingsIntoForm() {
  currentSettings = await invoke("get_settings");
  setGameDetection.checked = currentSettings.game_detection;
  setWorkDetection.checked = currentSettings.work_detection;
  setDownloadDetection.checked = currentSettings.download_detection;
  setLlmDetection.checked = currentSettings.llm_detection ?? true;
  setLlmBrief.checked = currentSettings.llm_brief ?? true;
  setPagePreview.checked = currentSettings.page_preview;
  setStartWithWindows.checked = currentSettings.start_with_windows;
  setIdleHideDelay.value = currentSettings.idle_hide_delay_s;
  setIdleHideDelayLabel.textContent = `${currentSettings.idle_hide_delay_s}s`;
  setShowAtCursor.checked = currentSettings.show_at_cursor;
  setCursorFollow.checked = currentSettings.cursor_follow;
  setReactToAudio.checked = currentSettings.react_to_audio;
  setCompactWidth.value = currentSettings.compact_width ?? 100;
  setCompactWidthLabel.textContent = `${setCompactWidth.value}%`;
  setPinShrink.value = currentSettings.pin_shrink ?? 75;
  setPinShrinkLabel.textContent = pinShrinkText(setPinShrink.value);
  setEdgeDwell.value = currentSettings.edge_dwell_ms;
  setEdgeDwellLabel.textContent = edgeDwellText(currentSettings.edge_dwell_ms);
  setPeekDuration.value = currentSettings.peek_duration_s;
  setPeekDurationLabel.textContent = `${currentSettings.peek_duration_s}s`;
  setCalendarUrl.value = currentSettings.calendar_ics_url;
  setCalendarUrl.closest(".field").classList.toggle("filled", setCalendarUrl.value !== "");
  setCalendarLead.value = currentSettings.calendar_reminder_lead_min;
  setCalendarLeadLabel.textContent = `${currentSettings.calendar_reminder_lead_min}m`;
  setAudioBleed.value = currentSettings.audio_bleed ?? 60;
  setAudioBleedLabel.textContent = `${setAudioBleed.value}%`;
  bleedLevel = Number(setAudioBleed.value) / 100;
  setGlow.value = currentSettings.glow_intensity ?? 70;
  setGlowLabel.textContent = `${setGlow.value}%`;
  applyGlow(Number(setGlow.value));
  setBgDim.value = currentSettings.bg_dim;
  setBgDimLabel.textContent = `${currentSettings.bg_dim}%`;
  applyBackgrounds(currentSettings);
}

function saveSettingsFromForm() {
  if (!currentSettings) return;
  currentSettings = {
    ...currentSettings,
    game_detection: setGameDetection.checked,
    work_detection: setWorkDetection.checked,
    download_detection: setDownloadDetection.checked,
    llm_detection: setLlmDetection.checked,
    llm_brief: setLlmBrief.checked,
    page_preview: setPagePreview.checked,
    start_with_windows: setStartWithWindows.checked,
    idle_hide_delay_s: Number(setIdleHideDelay.value),
    show_at_cursor: setShowAtCursor.checked,
    cursor_follow: setCursorFollow.checked,
    react_to_audio: setReactToAudio.checked,
    peek_duration_s: Number(setPeekDuration.value),
    edge_dwell_ms: Number(setEdgeDwell.value),
    pin_shrink: Number(setPinShrink.value),
    compact_width: Number(setCompactWidth.value),
    calendar_ics_url: setCalendarUrl.value.trim(),
    calendar_reminder_lead_min: Number(setCalendarLead.value),
    bg_dim: Number(setBgDim.value),
    glow_intensity: Number(setGlow.value),
    audio_bleed: Number(setAudioBleed.value),
  };
  invoke("save_settings", { settings: currentSettings });
}

for (const el of [setGameDetection, setWorkDetection, setDownloadDetection, setLlmDetection, setLlmBrief, setPagePreview, setStartWithWindows, setShowAtCursor, setCursorFollow, setReactToAudio]) {
  el.addEventListener("change", saveSettingsFromForm);
}
setIdleHideDelay.addEventListener("input", () => {
  setIdleHideDelayLabel.textContent = `${setIdleHideDelay.value}s`;
});
setIdleHideDelay.addEventListener("change", saveSettingsFromForm);
setCompactWidth.addEventListener("input", () => {
  setCompactWidthLabel.textContent = `${setCompactWidth.value}%`;
});
setCompactWidth.addEventListener("change", saveSettingsFromForm);
setPinShrink.addEventListener("input", () => {
  setPinShrinkLabel.textContent = pinShrinkText(setPinShrink.value);
});
setPinShrink.addEventListener("change", saveSettingsFromForm);
setEdgeDwell.addEventListener("input", () => {
  setEdgeDwellLabel.textContent = edgeDwellText(setEdgeDwell.value);
});
setEdgeDwell.addEventListener("change", saveSettingsFromForm);
setPeekDuration.addEventListener("input", () => {
  setPeekDurationLabel.textContent = `${setPeekDuration.value}s`;
});
setPeekDuration.addEventListener("change", saveSettingsFromForm);
setCalendarLead.addEventListener("input", () => {
  setCalendarLeadLabel.textContent = `${setCalendarLead.value}m`;
});
setCalendarLead.addEventListener("change", saveSettingsFromForm);
setCalendarUrl.addEventListener("change", saveSettingsFromForm);
// the clear button shows only while the field holds something
function syncCalendarField() {
  setCalendarUrl.closest(".field").classList.toggle("filled", setCalendarUrl.value !== "");
}
setCalendarUrl.addEventListener("input", syncCalendarField);
document.getElementById("set-calendar-clear").addEventListener("click", () => {
  setCalendarUrl.value = "";
  syncCalendarField();
  saveSettingsFromForm();
  setCalendarUrl.focus();
});
function applyGlow(pct) {
  document.documentElement.style.setProperty("--gi", (pct / 100).toFixed(2));
}
setAudioBleed.addEventListener("input", () => {
  setAudioBleedLabel.textContent = `${setAudioBleed.value}%`;
  bleedLevel = Number(setAudioBleed.value) / 100; // live: the next frame uses it
});
setAudioBleed.addEventListener("change", saveSettingsFromForm);
let glowPreviewTimer = 0;
setGlow.addEventListener("input", () => {
  setGlowLabel.textContent = `${setGlow.value}%`;
  applyGlow(Number(setGlow.value)); // live preview while dragging
  document.body.classList.add("glow-preview");
  clearTimeout(glowPreviewTimer);
  glowPreviewTimer = setTimeout(() => document.body.classList.remove("glow-preview"), 900);
});
setGlow.addEventListener("change", saveSettingsFromForm);
setBgDim.addEventListener("input", () => {
  setBgDimLabel.textContent = `${setBgDim.value}%`;
  applyBgDim(Number(setBgDim.value)); // live preview while dragging
});
setBgDim.addEventListener("change", saveSettingsFromForm);

// -- hub calendar: upcoming events from the .ics feed, pushed by Rust --
const calendarStatusEl = document.getElementById("calendar-status");
const calendarEventsEl = document.getElementById("calendar-events");

function formatEventTime(ev) {
  const d = new Date(ev.start_ms);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const tomorrow = new Date(now.getTime() + 86400000).toDateString() === d.toDateString();
  const day = sameDay
    ? "Today"
    : tomorrow
      ? "Tomorrow"
      : d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
  if (ev.all_day) return day;
  return `${day} ${d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
}

function applyCalendar(c) {
  calSetData(c);
  calendarEventsEl.textContent = "";
  calendarStatusEl.textContent = "";
  if (!c || !c.configured) {
    updateInfoEmpty();
    return;
  }
  if (c.error) {
    calendarStatusEl.textContent = c.error;
    updateInfoEmpty();
    return;
  }
  // the feed now carries past + recurring events for the calendar view; the
  // info list only wants what's still ahead
  const upcoming = c.events.filter((ev) => ev.end_ms >= Date.now() - 3600000).slice(0, 20);
  for (const ev of upcoming) {
    const row = document.createElement("div");
    row.className = "calendar-event spot";
    const title = document.createElement("span");
    title.className = "calendar-event-title";
    title.textContent = ev.summary;
    const time = document.createElement("span");
    time.className = "calendar-event-time";
    withIcon(time, "clock", formatEventTime(ev));
    row.append(title, time);
    calendarEventsEl.append(row);
  }
  updateInfoEmpty();
}


// -- hub calendar view: click the clock to swap the cards for a month grid.
// Pick a day to list that day's events from the .ics feed. Collapsing the
// island drops back to the cards (the hub always reopens on the info pane). --
const calTitleEl = document.getElementById("cal-title");
const calWeekEl = document.getElementById("cal-week");
const calGridEl = document.getElementById("cal-grid");
const calDayTitleEl = document.getElementById("cal-day-title");
const calStatusEl = document.getElementById("cal-status");
const calEventsEl = document.getElementById("cal-events");
const cal = { data: null, byDay: new Map(), month: null, sel: null };

const dayKey = (d) => d.getFullYear() * 10000 + (d.getMonth() + 1) * 100 + d.getDate();
const startOfDay = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate());
const addDays = (d, n) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);

// first weekday of the user's locale: 0 = Sunday .. 6 = Saturday
function calFirstDow() {
  try {
    const loc = new Intl.Locale(navigator.language);
    const info = loc.getWeekInfo ? loc.getWeekInfo() : loc.weekInfo;
    if (info && info.firstDay) return info.firstDay % 7;
  } catch (_) {}
  return 0;
}
const CAL_FIRST_DOW = calFirstDow();

function calIndex() {
  cal.byDay = new Map();
  if (!cal.data || !cal.data.events) return;
  for (const ev of cal.data.events) {
    const first = startOfDay(new Date(ev.start_ms));
    // end is exclusive: a 00:00-00:00 all-day event covers exactly one day
    const lastMs = ev.end_ms > ev.start_ms ? ev.end_ms - 1 : ev.start_ms;
    const last = startOfDay(new Date(lastMs));
    let d = first;
    for (let n = 0; d <= last && n < 62; n++, d = addDays(d, 1)) {
      const k = dayKey(d);
      if (!cal.byDay.has(k)) cal.byDay.set(k, []);
      cal.byDay.get(k).push(ev);
    }
  }
}

function calSetData(c) {
  cal.data = c;
  calIndex();
  if (!paneCalendar.classList.contains("hidden")) {
    buildCalGrid(null);
    renderCalDay(false);
  }
}

function buildCalWeekHeader() {
  calWeekEl.textContent = "";
  const base = new Date(2023, 0, 1); // a Sunday
  for (let i = 0; i < 7; i++) {
    const s = document.createElement("span");
    s.textContent = addDays(base, (CAL_FIRST_DOW + i) % 7).toLocaleDateString([], { weekday: "narrow" });
    calWeekEl.append(s);
  }
}
buildCalWeekHeader();

function buildCalGrid(dir) {
  const first = new Date(cal.month.getFullYear(), cal.month.getMonth(), 1);
  calTitleEl.textContent = first.toLocaleDateString([], { month: "long", year: "numeric" });
  const offset = (first.getDay() - CAL_FIRST_DOW + 7) % 7;
  const gridStart = addDays(first, -offset);
  const todayKey = dayKey(new Date());
  const selKey = dayKey(cal.sel);
  calGridEl.textContent = "";
  for (let i = 0; i < 42; i++) {
    const d = addDays(gridStart, i);
    const k = dayKey(d);
    const btn = document.createElement("button");
    btn.className = "cal-day";
    if (d.getMonth() !== first.getMonth()) btn.classList.add("other");
    if (k === todayKey) btn.classList.add("today");
    if (k === selKey) btn.classList.add("sel");
    btn.dataset.key = k;
    const num = document.createElement("span");
    num.textContent = d.getDate();
    const dots = document.createElement("span");
    dots.className = "cal-dots";
    const n = Math.min((cal.byDay.get(k) || []).length, 3);
    for (let j = 0; j < n; j++) dots.append(document.createElement("i"));
    btn.append(num, dots);
    btn.addEventListener("click", () => selectCalDay(d));
    calGridEl.append(btn);
  }
  if (dir) {
    calGridEl.classList.remove("slide-next", "slide-prev");
    void calGridEl.offsetWidth; // restart the slide
    calGridEl.classList.add(dir > 0 ? "slide-next" : "slide-prev");
  }
}

function calTimeLabel(ev, day) {
  if (ev.all_day) return "All day";
  const fmt = (ms) => new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const dayStart = day.getTime();
  const dayEnd = addDays(day, 1).getTime();
  const startsToday = ev.start_ms >= dayStart;
  const endsToday = ev.end_ms <= dayEnd;
  if (ev.end_ms <= ev.start_ms) return fmt(ev.start_ms);
  if (startsToday && endsToday) return `${fmt(ev.start_ms)} – ${fmt(ev.end_ms)}`;
  if (startsToday) return `${fmt(ev.start_ms)} →`;
  return endsToday ? `→ ${fmt(ev.end_ms)}` : "All day";
}

function renderCalDay(animate) {
  const day = startOfDay(cal.sel);
  const list = (cal.byDay.get(dayKey(day)) || []).slice().sort((a, b) => {
    return (b.all_day ? 1 : 0) - (a.all_day ? 1 : 0) || a.start_ms - b.start_ms;
  });
  const label = day.toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
  // a day with nothing on it shows nothing: no date heading, no placeholder
  calDayTitleEl.textContent = list.length ? `${label} · ${list.length} ${list.length === 1 ? "event" : "events"}` : "";

  calStatusEl.textContent = "";
  calEventsEl.textContent = "";
  const d = cal.data;
  if (!d || !d.configured) {
    return; // no feed set up: just an empty day, no hint text
  }
  if (d.error) {
    calStatusEl.textContent = d.error;
    return;
  }
  if (!list.length) return;
  const now = Date.now();
  list.forEach((ev, i) => {
    const row = document.createElement("div");
    row.className = "cal-event spot";
    if (ev.end_ms < now && !(ev.all_day && ev.end_ms >= now)) row.classList.add("past");
    if (animate) {
      row.classList.add("fresh");
      row.style.setProperty("--i", i);
    }
    const title = document.createElement("div");
    title.className = "cal-event-title";
    title.textContent = ev.summary;
    const time = document.createElement("div");
    time.className = "cal-event-time";
    withIcon(time, "clock", calTimeLabel(ev, day));
    row.append(title, time);
    calEventsEl.append(row);
  });
}

function selectCalDay(d) {
  const prevMonth = cal.month.getFullYear() * 12 + cal.month.getMonth();
  cal.sel = startOfDay(d);
  const newMonth = d.getFullYear() * 12 + d.getMonth();
  if (newMonth !== prevMonth) {
    cal.month = new Date(d.getFullYear(), d.getMonth(), 1);
    buildCalGrid(newMonth > prevMonth ? 1 : -1);
  } else {
    // same month: just move the highlight (the disc springs between cells)
    const k = String(dayKey(cal.sel));
    for (const el of calGridEl.children) el.classList.toggle("sel", el.dataset.key === k);
  }
  renderCalDay(true);
}

function shiftCalMonth(delta) {
  const prev = cal.month;
  cal.month = new Date(prev.getFullYear(), prev.getMonth() + delta, 1);
  // keep the same day-of-month selected where it exists (clamped to month end)
  const lastDay = new Date(cal.month.getFullYear(), cal.month.getMonth() + 1, 0).getDate();
  cal.sel = new Date(cal.month.getFullYear(), cal.month.getMonth(), Math.min(cal.sel.getDate(), lastDay));
  buildCalGrid(delta);
  renderCalDay(true);
}

function openCalendar() {
  const today = startOfDay(new Date());
  cal.sel = today;
  cal.month = new Date(today.getFullYear(), today.getMonth(), 1);
  buildCalGrid(null);
  renderCalDay(false);
  setHubPane("calendar");
}

document.getElementById("hub-clock").addEventListener("click", () => {
  mood.happyUntil = performance.now() + 700;
  if (paneCalendar.classList.contains("hidden")) openCalendar();
  else setHubPane("info");
});
document.getElementById("cal-prev").addEventListener("click", () => shiftCalMonth(-1));
document.getElementById("cal-next").addEventListener("click", () => shiftCalMonth(1));
calTitleEl.addEventListener("click", () => {
  const today = startOfDay(new Date());
  if (dayKey(today) === dayKey(cal.sel) && cal.month.getMonth() === today.getMonth()) return;
  const dir = today.getFullYear() * 12 + today.getMonth() > cal.month.getFullYear() * 12 + cal.month.getMonth() ? 1 : -1;
  cal.sel = today;
  cal.month = new Date(today.getFullYear(), today.getMonth(), 1);
  buildCalGrid(dir);
  renderCalDay(true);
});
let calWheelAt = 0;
paneCalendar.addEventListener(
  "wheel",
  (e) => {
    if (!e.target.closest("#cal-grid, #cal-head") || performance.now() - calWheelAt < 260) return;
    calWheelAt = performance.now();
    e.preventDefault();
    shiftCalMonth(e.deltaY > 0 ? 1 : -1);
  },
  { passive: false },
);
// clicks in here must not reach #pill's mousedown (it would start a window
// drag / read the click as "toggle the hub")
paneCalendar.addEventListener("mousedown", (e) => e.stopPropagation());
document.getElementById("hub-clock").addEventListener("mousedown", (e) => e.stopPropagation());

async function loadCalendar() {
  applyCalendar(await invoke("get_calendar"));
}
listen("calendar-tick", (e) => applyCalendar(e.payload));

// -- custom backgrounds: an image or video behind the collapsed island, and
// optionally a different one behind the expanded hub. Files live in the app's
// data folder and load via the asset protocol (video streams with range
// requests). Videos only play while the island is actually on screen, so a
// hidden island costs no decoding. --
const bgLayers = {
  compact: { el: document.getElementById("bg-compact"), path: "", media: null },
  hub: { el: document.getElementById("bg-hub"), path: "", media: null },
};
let bgDim = 0.5;
let islandShown = false;
const BG_VIDEO_RE = /\.(mp4|webm|mov|m4v)$/i;

function bgLabel(path) {
  // stored as "<kind>-<timestamp>-<original name>.<ext>"
  const file = path.split(/[\\/]/).pop() || "";
  return file.replace(/^(compact|hub)-\d+-/, "");
}

function setBgLayer(layer, path) {
  if (layer.path === path) return;
  layer.path = path;
  layer.el.textContent = "";
  layer.media = null;
  if (!path) return;
  const src = window.__TAURI__.core.convertFileSrc(path);
  let media;
  if (BG_VIDEO_RE.test(path)) {
    media = document.createElement("video");
    media.muted = true;
    media.loop = true;
    media.playsInline = true;
    media.disablePictureInPicture = true;
    media.preload = "auto";
  } else {
    media = document.createElement("img");
    media.alt = "";
    media.decoding = "async";
    media.draggable = false;
  }
  media.src = src;
  layer.media = media;
  layer.el.append(media);
}

function applyBgDim(pct) {
  bgDim = pct / 100;
  bgLayers.compact.el.style.setProperty("--dim", bgDim);
  // the hub is mostly text: a little more scrim than the compact views
  bgLayers.hub.el.style.setProperty("--dim", Math.min(0.92, bgDim + 0.12));
}

// which layer shows, and which videos are allowed to run
function updateBg() {
  const hubWanted = currentView === "hub" && !!bgLayers.hub.path;
  const compactWanted = !!bgLayers.compact.path && !hubWanted;
  bgLayers.hub.el.classList.toggle("active", hubWanted);
  bgLayers.compact.el.classList.toggle("active", compactWanted);
  const visible = islandShown && !document.hidden;
  for (const [layer, wanted] of [
    [bgLayers.hub, hubWanted],
    [bgLayers.compact, compactWanted],
  ]) {
    const v = layer.media;
    if (!v || v.tagName !== "VIDEO") continue;
    if (wanted && visible) v.play().catch(() => {});
    else v.pause();
  }
}

function applyBackgrounds(s) {
  if (!s) return;
  setBgLayer(bgLayers.compact, s.bg_compact || "");
  setBgLayer(bgLayers.hub, s.bg_hub || "");
  applyBgDim(s.bg_dim ?? 50);
  applyGlow(s.glow_intensity ?? 70);
  bleedLevel = (s.audio_bleed ?? 60) / 100;
  updateBg();
  for (const kind of ["compact", "hub"]) {
    const path = s["bg_" + kind] || "";
    document.getElementById(`bg-name-${kind}`).textContent = path ? bgLabel(path) : "None";
    document.getElementById(`bg-pick-${kind}`).textContent = path ? "Change" : "Choose";
    document.getElementById(`bg-clear-${kind}`).classList.toggle("hidden", !path);
  }
}

const bgErrorEl = document.getElementById("bg-error");
for (const kind of ["compact", "hub"]) {
  document.getElementById(`bg-pick-${kind}`).addEventListener("click", async () => {
    bgErrorEl.textContent = "";
    try {
      const s = await invoke("pick_background", { kind });
      if (s) {
        currentSettings = s;
        applyBackgrounds(s);
      }
    } catch (err) {
      bgErrorEl.textContent = String(err);
    }
  });
  document.getElementById(`bg-clear-${kind}`).addEventListener("click", async () => {
    bgErrorEl.textContent = "";
    currentSettings = await invoke("clear_background", { kind });
    applyBackgrounds(currentSettings);
  });
}
for (const id of ["bg-pick-compact", "bg-pick-hub", "bg-clear-compact", "bg-clear-hub"]) {
  document.getElementById(id).addEventListener("mousedown", (e) => e.stopPropagation());
}
document.addEventListener("visibilitychange", updateBg);
// at startup: the backgrounds apply before the hub (and its form) is ever opened
invoke("get_settings").then(applyBackgrounds);

// every settings row is a <label>, so clicking anywhere on it (not just the
// control) toggles/focuses it -- that click also bubbles up to #pill's own
// mousedown handler, which would otherwise read it as "click the hub
// background" and close the hub right as the user tries to change a setting
for (const row of document.querySelectorAll(".hub-row")) {
  row.addEventListener("mousedown", (e) => e.stopPropagation());
}

// -- hub stat rings: Claude 5h/7d usage (pushed from Rust every 2min, click
// to refresh with a 60s cooldown enforced Rust-side) and CPU/RAM (polled
// each second while the hub is open). Percent lives in the tooltip only. --
const RING_R = 8;
const RING_C = 2 * Math.PI * RING_R;

const ringEls = {};
for (const el of document.querySelectorAll(".ring-wrap")) {
  const key = el.id.replace("ring-", "");
  const color = el.dataset.color;
  el.innerHTML =
    `<svg viewBox="0 0 20 20">` +
    `<circle class="ring-bg" cx="10" cy="10" r="${RING_R}"></circle>` +
    `<circle class="ring-fg" cx="10" cy="10" r="${RING_R}" stroke="${color}" stroke-dasharray="0 ${RING_C}"></circle>` +
    `</svg>`;
  ringEls[key] = { el, fg: el.querySelector(".ring-fg") };
  // rings sit on the click-toggles-hub pill; don't let a ring click close it
  el.addEventListener("mousedown", (e) => e.stopPropagation());
}

function setRing(key, pct, tooltip) {
  const ring = ringEls[key];
  if (!ring) return;
  const clamped = Math.max(0, Math.min(100, pct ?? 0));
  const len = (clamped / 100) * RING_C;
  ring.fg.setAttribute("stroke-dasharray", `${len} ${RING_C}`);
  ring.el.dataset.tip = tooltip;
  refreshTip(ring.el); // live-update if it is showing right now
}

function formatReset(iso) {
  if (!iso) return "";
  const mins = Math.max(0, Math.floor((new Date(iso).getTime() - Date.now()) / 60000));
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ${mins % 60}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

function applyUsage(u) {
  const windows = [
    ["five_hour", "5h", u?.five_hour],
    ["seven_day", "7d", u?.seven_day],
  ];
  for (const [key, label, w] of windows) {
    const ring = ringEls[key];
    const visible = !!(u && u.available && !u.error && w && w.pct != null);
    ring.el.classList.toggle("hidden", !visible);
    ring.el.classList.toggle("clickable", visible);
    if (!visible) continue;
    const reset = formatReset(w.resets_at);
    const tip = `Claude ${label} · ${Math.round(w.pct)}%\n${reset ? `Resets in ${reset} · ` : ""}Click to refresh`;
    setRing(key, w.pct, tip);
  }
}

listen("usage-tick", (event) => applyUsage(event.payload));
for (const key of ["five_hour", "seven_day"]) {
  ringEls[key].el.addEventListener("click", () => invoke("refresh_usage"));
}

async function pollSysStats() {
  if (currentView !== "hub") return;
  const s = await invoke("get_sys_stats");
  setRing("cpu", s.cpu_pct, `CPU · ${Math.round(s.cpu_pct)}%`);
  setRing(
    "ram",
    s.ram_pct,
    `RAM · ${Math.round(s.ram_pct)}%\n${s.ram_used_gb.toFixed(1)} / ${s.ram_total_gb.toFixed(1)} GB`
  );
}
setInterval(pollSysStats, 1000);

// GPU: Windows perf counters (Rust-side); null until the counter has a
// baseline or if the machine has no GPU counters -- ring stays hidden then
async function pollGpu() {
  if (currentView !== "hub") return;
  const pct = await invoke("get_gpu_pct");
  const ring = ringEls.gpu;
  if (pct == null) return;
  ring.el.classList.remove("hidden");
  setRing("gpu", pct, `GPU · ${Math.round(pct)}%`);
}
setInterval(pollGpu, 1000);

async function loadUsage() {
  applyUsage(await invoke("get_usage"));
}


// -- hub "Now" section: media card (live, from media-tick) + game / work /
// coding cards (polled from Rust while the hub is showing the info pane) --
const hubMediaEl = document.getElementById("hub-media");
const hubMediaArt = document.getElementById("hub-media-art");
const hubMediaTitle = document.getElementById("hub-media-title");
const hubMediaArtist = document.getElementById("hub-media-artist");
const hubMediaPlay = document.getElementById("hub-media-play");
const hubMediaPrev = document.getElementById("hub-media-prev");
const hubMediaNext = document.getElementById("hub-media-next");
const hubMediaSeek = document.getElementById("hub-media-seek");
const hubMediaSeekFill = document.getElementById("hub-media-seek-fill");
const nowCardsEl = document.getElementById("now-cards");
// cards the user has opened (all start collapsed); kept by key so a re-render keeps them open
const openCards = new Set();

hubMediaPrev.innerHTML = ICON.prev;
hubMediaNext.innerHTML = ICON.next;
let hubMediaDuration = 0;
let hubMediaSource = "";
let hubMediaTitleText = "";
let hubHasMedia = false;
let hubHasActivity = false;

// (an empty list just stays empty -- no placeholder text; callers still call this)
function updateInfoEmpty() {}

function applyHubMedia(m) {
  hubHasMedia = !!(m && m.has_session);
  hubMediaEl.classList.toggle("hidden", !hubHasMedia);
  updateInfoEmpty();
  if (!hubHasMedia) return;
  hubMediaTitle.textContent = m.title || "";
  hubMediaSource = m.source || "";
  hubMediaTitleText = m.title || "";
  hubMediaArtist.textContent = m.artist || "";
  if (m.art) hubMediaArt.src = m.art;
  hubMediaArt.classList.toggle("hidden", !m.art);
  setPlayIcon(hubMediaPlay, m.playing);
  hubMediaPrev.classList.toggle("hidden", !m.can_previous);
  hubMediaNext.classList.toggle("hidden", !m.can_next);
  // live streams report no duration -- nothing to seek in, so no bar
  hubMediaDuration = m.duration || 0;
  hubMediaSeek.classList.toggle("hidden", !(hubMediaDuration > 0));
  if (hubMediaDuration > 0) {
    hubMediaSeekFill.style.width = `${Math.min(100, ((m.position || 0) / hubMediaDuration) * 100)}%`;
  }
}

for (const btn of [hubMediaPlay, hubMediaPrev, hubMediaNext, hubMediaSeek]) {
  btn.addEventListener("mousedown", (e) => e.stopPropagation());
}
// clicking the card itself (not its buttons / seek bar) jumps to whoever is playing
hubMediaEl.classList.add("clickable");
collapsible(hubMediaEl, "media");
hubMediaEl.addEventListener("mousedown", (e) => e.stopPropagation());
hubMediaEl.addEventListener("click", (e) => {
  if (e.target.closest("button, #hub-media-seek, .now-row")) return; // the header toggles the card
  invoke("focus_source", { titleHint: hubMediaTitleText, sourceId: hubMediaSource });
});
hubMediaEl.addEventListener("dblclick", (e) => {
  if (hubMediaEl._inHeader(e)) invoke("focus_source", { titleHint: hubMediaTitleText, sourceId: hubMediaSource });
});
hubMediaPlay.addEventListener("click", () => invoke("media_play_pause"));
hubMediaPrev.addEventListener("click", () => invoke("media_previous"));
hubMediaNext.addEventListener("click", () => invoke("media_next"));
hubMediaSeek.addEventListener("click", (e) => {
  if (!(hubMediaDuration > 0)) return;
  const rect = hubMediaSeek.getBoundingClientRect();
  const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  invoke("media_seek", { positionSeconds: frac * hubMediaDuration });
  hubMediaSeekFill.style.width = `${frac * 100}%`;
});

const WORK_CARD_LABEL = {
  coding: "Coding",
  writing: "Writing",
  design: "Designing",
  communication: "Messaging",
  media: "Watching/Listening",
  gaming: "Gaming",
  browsing: "Browsing",
};

function formatDuration(secs) {
  const s = Math.max(0, Math.floor(secs));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m`;
  return "<1m";
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function iconEl(icon, glyph) {
  if (icon) {
    const img = el("img", "now-icon");
    img.src = icon;
    img.alt = "";
    return img;
  }
  const box = el("div", "now-icon glyph");
  box.innerHTML = li(glyph);
  return box;
}

// `v` is a string, or { icon, text } for a short text that an icon explains
function line(cls, v) {
  const node = el("div", cls);
  if (v && typeof v === "object") withIcon(node, v.icon, v.text);
  else node.textContent = v;
  return node;
}

function textCol(title, sub) {
  const col = el("div", "now-text");
  col.append(line("now-title", title), line("now-sub", sub));
  return col;
}

// how long something has been going: a stopwatch and the time, no words
const going = (secs) => ({ icon: "timer", text: formatDuration(secs) });

// Every card has a collapsed state (the default): its header only. A chevron opens it to show
// the rest. Call this once the card is fully built; `key` identifies it across refreshes.
function collapsible(card, key) {
  // the signature that decides whether the list must be rebuilt ignores the open/closed state
  if (card._sig === undefined) card._sig = card.outerHTML;
  card.classList.add("collapsible");
  if (openCards.has(key)) card.classList.add("open");
  const btn = el("button", "card-toggle");
  btn.innerHTML = li("chevron");
  const sync = () => {
    btn.title = card.classList.contains("open") ? "Collapse" : "Expand";
  };
  sync();
  // the whole header toggles it (the chevron only shows the state); the header's own
  // buttons (media controls) keep doing their own thing
  const row = card.querySelector(".now-row") || card;
  row.classList.add("toggles");
  // the header is the row plus the card's padding around it: no dead spots
  card._inHeader = (e) => {
    if (e.target.closest("button:not(.card-toggle), .now-seek, .work-body, .dl-list")) return false;
    const r = row.getBoundingClientRect();
    return e.clientY <= r.bottom + 12;
  };
  card.addEventListener("click", (e) => {
    if (!card._inHeader(e)) return;
    e.stopImmediatePropagation(); // not "go to the app"
    const open = card.classList.toggle("open");
    if (open) openCards.add(key);
    else openCards.delete(key);
    sync();
    scheduleHubHeight();
  });
  row.append(btn);
  return card;
}

// What a collapsed card still says at the right of its header (the sub-line is hidden then);
// `v` is a string or { icon, text } like `line`. Call before `collapsible`.
function peek(card, v) {
  const row = card.querySelector(".now-row");
  if (row && v) row.append(line("now-peek", v));
  return card;
}

// a card that jumps to its source app when clicked (mousedown must not reach
// #pill, whose handler would read the click as "toggle the hub")
function focusable(card, args, go = () => invoke("focus_source", args)) {
  card.classList.add("clickable");
  card.addEventListener("mousedown", (e) => e.stopPropagation());
  // the header opens/closes the card (see collapsible); the rest of the card, or a
  // double-click on the header, goes to the app
  card.addEventListener("click", (e) => {
    if (!(card._inHeader ? card._inHeader(e) : e.target.closest(".now-row"))) go();
  });
  card.addEventListener("dblclick", (e) => {
    if (card._inHeader ? card._inHeader(e) : e.target.closest(".now-row")) go();
  });
  return card;
}

// What each running game uses (CPU, memory, GPU, video memory), by pid; filled by refreshActivity
const gameStats = new Map();

// a small progress ring like the ones at the top right of the hub
function statRing(color) {
  const el = document.createElement("span");
  el.className = "ring-wrap hidden";
  el.innerHTML = `<svg viewBox="0 0 20 20"><circle class="ring-bg" cx="10" cy="10" r="${RING_R}"></circle><circle class="ring-fg" cx="10" cy="10" r="${RING_R}" stroke="${color}" stroke-dasharray="0 ${RING_C}"></circle></svg>`;
  const fg = el.querySelector(".ring-fg");
  return {
    el,
    set(pct, tip) {
      if (pct == null) {
        el.classList.add("hidden");
        return;
      }
      el.classList.remove("hidden");
      fg.setAttribute("stroke-dasharray", `${(Math.max(0, Math.min(100, pct)) / 100) * RING_C} ${RING_C}`);
      el.dataset.tip = tip;
      refreshTip(el);
    },
  };
}

// frame times (ms, newest last) as a line: the 60 fps (16.7 ms) and 30 fps (33.3 ms) levels are marked,
// and the scale is fixed at 50 ms so a hitch is a visible spike
function frameGraph(ms) {
  if (!ms || ms.length < 2) return "";
  const max = 50;
  const pts = ms.map((v, i) => [(i / (ms.length - 1)) * 120, 52 - Math.min(v / max, 1) * 50]);
  const line = pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");
  const y = (v) => (52 - (v / max) * 50).toFixed(1);
  return `<svg viewBox="0 0 120 54" preserveAspectRatio="none"><line class="g" x1="0" x2="120" y1="${y(16.7)}" y2="${y(16.7)}"/><line class="g" x1="0" x2="120" y1="${y(33.3)}" y2="${y(33.3)}"/><path class="a" d="${line} L120 54 L0 54 Z"/><path class="l" d="${line}"/></svg>`;
}

// Game: collapsed it is the name and three rings (GPU, CPU, memory -- the game's own share, in the
// colours of the hub's rings); opened it adds the meters and the time played.
function gameCard(g) {
  const card = el("div", "now-card spot");
  const row = el("div", "now-row");
  row.append(iconEl(g.icon, "gamepad"), textCol(g.name, going(g.playtime_secs)));
  const rings = el("div", "g-rings");
  const gpu = statRing("#8ADB6E");
  const cpu = statRing("#5AC8E6");
  const ram = statRing("#B47EE6");
  // frames per second, when they can be counted: the number leads, the rings follow
  const fpsEl = el("span", "g-fps");
  fpsEl.hidden = true;
  const fpsNum = el("b");
  fpsEl.append(fpsNum, el("small", "", "FPS"));
  rings.append(fpsEl, gpu.el, cpu.el, ram.el);
  row.append(rings);
  card.append(row);

  // opened: FPS, 1% low and frame time, the frame-time graph, then the meters
  const vitals = el("div", "work-body g-vitals");
  vitals.hidden = true;
  const stat = (label) => {
    const box = el("div", "g-stat");
    const num = el("b");
    box.append(num, el("span", "", label));
    return { box, num };
  };
  const sFps = stat("FPS");
  const sLow = stat("1% low");
  const sMs = stat("Frame time");
  const statsRow = el("div", "g-stats");
  statsRow.append(sFps.box, sLow.box, sMs.box);
  const graph = el("div", "g-graph");
  vitals.append(statsRow, graph);
  card.append(vitals);

  const body = el("div", "work-body g-meters");
  const meters = {};
  for (const [key, label, color] of [["gpu", "GPU", "#8ADB6E"], ["vram", "VRAM", "#8ADB6E"], ["cpu", "CPU", "#5AC8E6"], ["ram", "RAM", "#B47EE6"]]) {
    const m = el("div", "g-meter");
    const bar = el("i");
    bar.style.setProperty("--c", color);
    const val = el("em");
    m.append(el("span", "", label), bar, val);
    body.append(m);
    meters[key] = { m, bar, val };
  }
  card.append(body);

  const update = (data) => {
    const s = gameStats.get(data.pid);
    const sub = card.querySelector(".now-sub");
    if (sub) withIcon(sub, "timer", formatDuration(data.playtime_secs));
    const put = (key, pct, text, tip) => {
      const mt = meters[key];
      mt.m.hidden = pct == null;
      mt.bar.style.setProperty("--v", `${Math.max(0, Math.min(100, pct ?? 0))}%`);
      mt.val.textContent = text;
    };
    const hasFps = !!(s && s.fps != null);
    fpsEl.hidden = !hasFps;
    vitals.hidden = !hasFps;
    if (hasFps) {
      fpsNum.textContent = String(Math.round(s.fps));
      sFps.num.textContent = String(Math.round(s.fps));
      sLow.num.textContent = String(Math.round(s.low_fps));
      sMs.num.textContent = `${(1000 / Math.max(s.fps, 1)).toFixed(1)} ms`;
      graph.innerHTML = frameGraph(s.frame_ms);
    }
    gpu.set(s ? s.gpu_pct : null, s && s.gpu_pct != null ? `GPU \u00b7 ${Math.round(s.gpu_pct)}%` : "");
    cpu.set(s ? s.cpu_pct : null, s ? `CPU \u00b7 ${Math.round(s.cpu_pct)}%` : "");
    ram.set(s ? s.ram_pct : null, s ? `RAM \u00b7 ${s.ram_gb.toFixed(1)} GB` : "");
    put("gpu", s ? s.gpu_pct : null, s && s.gpu_pct != null ? `${Math.round(s.gpu_pct)}%` : "");
    put("vram", s && s.vram_gb != null ? Math.min(100, (s.vram_gb / 12) * 100) : null, s && s.vram_gb != null ? `${s.vram_gb.toFixed(1)} GB` : "");
    put("cpu", s ? s.cpu_pct : null, s ? `${Math.round(s.cpu_pct)}%` : "");
    put("ram", s ? s.ram_pct : null, s ? `${s.ram_gb.toFixed(1)} GB` : "");
  };
  update(g);
  // rebuilt only when the game itself changes; the numbers update in place
  card._sig = `game:${g.pid}:${g.exe_path}`;
  card._data = g;
  card._update = update;
  return focusable(collapsible(card, `game:${g.exe_path || g.name}`), { exePath: g.exe_path });
}

function workCard(w) {
  if (w.category === "browsing" && w.browse) return browsingCard(w);
  const card = el("div", "now-card spot");
  const row = el("div", "now-row");
  const label = WORK_CARD_LABEL[w.category] || "Working";
  row.append(
    iconEl(w.icon, WORK_GLYPH[w.category] || "dot"),
    textCol({ icon: WORK_GLYPH[w.category] || "dot", text: w.app_name || label }, going(w.going_secs))
  );
  card.append(row);
  return focusable(collapsible(peek(card, going(w.going_secs)), `work:${w.category}`), { exePath: w.exe_path });
}

// A shortcut to a button of the page in front of you: the click is delivered to
// the browser window, so it works without leaving the island.
function pageButton(b) {
  const btn = el("button", "page-btn", b.label);
  const stop = (e) => e.stopPropagation();
  btn.addEventListener("mousedown", stop);
  btn.addEventListener("click", async (e) => {
    e.stopPropagation(); // not "go to the browser"
    btn.disabled = true;
    const ok = await invoke("click_page_button", { label: b.label, x: b.x, y: b.y });
    btn.classList.add(ok ? "done" : "fail");
    setTimeout(() => btn.classList.remove("done", "fail"), 900);
    btn.disabled = false;
  });
  return btn;
}

// Browsing: the page in front of you and what is in it, read from the page.
function browsingCard(w) {
  const b = w.browse;
  const pv = b.preview;
  const card = el("div", "now-card spot");
  const row = el("div", "now-row");
  row.append(iconEl(w.icon, "globe"));
  row.append(textCol(w.page || w.app_name || "Browsing", b.domain || going(w.going_secs)));
  card.append(row);

  const body = el("div", "work-body");
  if (pv) {
    if (pv.lead) body.append(el("div", "work-desc", pv.lead));
    if (pv.buttons.length) {
      const btns = el("div", "page-btns");
      for (const b of pv.buttons) btns.append(pageButton(b));
      body.append(btns);
    }
  }
  if (body.childElementCount) card.append(body);
  return focusable(collapsible(peek(card, b.domain || going(w.going_secs)), "web"), { exePath: w.exe_path });
}

function chip(text, cls) {
  return el("span", cls ? `chip ${cls}` : "chip", text);
}

// What you're working on: the project (with its git branch), the file in front
// of you and the uncommitted changes, summed up at the right edge.
const BRANCH_ICON =
  '<svg viewBox="0 0 16 16" width="10" height="10" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><circle cx="4.5" cy="3.5" r="1.7"/><circle cx="4.5" cy="12.5" r="1.7"/><circle cx="11.5" cy="5.5" r="1.7"/><path d="M4.5 5.2v5.6M11.5 7.2c0 3-4.5 2-7 4"/></svg>';

function codingCard(c) {
  const p = c.project_info && c.project_info.branch ? c.project_info : null;
  const card = el("div", "now-card spot");
  const row = el("div", "now-row");
  const tile = el("div", "now-icon tile");
  tile.innerHTML = li("code");
  row.append(tile);

  // the icon says it: a pen while editing a file, a slashed eye when nothing is going on
  // (the file itself is on its own line below)
  const status = c.active ? { icon: c.file ? "pen" : "code", text: "" } : { icon: "eyeoff", text: "" };
  const col = el("div", "now-text");
  const head = el("div", "work-head");
  head.append(el("span", "now-title", c.project || "Coding"));
  if (p) {
    const branch = chip("", "branch");
    branch.innerHTML = BRANCH_ICON;
    branch.append(document.createTextNode(` ${p.branch}`));
    head.append(branch);
  }
  col.append(head, line("now-sub", status));
  row.append(col);

  const total = el("div", "now-total");
  if (p && p.changed) {
    const nums = el("div", "now-total-value diff-sum");
    nums.append(el("span", "add", `+${p.added.toLocaleString()}`), el("span", "del", `\u2212${p.deleted.toLocaleString()}`));
    const files = el("div", "now-total-label");
    withIcon(files, "file", String(p.changed));
    total.append(nums, files);
  } else if (p) {
    const ok = el("div", "now-total-value");
    ok.innerHTML = li("check");
    ok.title = "No changes";
    total.append(ok);
  } else {
    // not a git project: fall back to the time spent
    const today = el("div", "now-total-label");
    today.innerHTML = li("clock");
    today.title = "Today";
    total.append(el("div", "now-total-value", formatDuration(c.today_secs)), today);
  }
  row.append(total);
  card.append(row);

  const body = el("div", "work-body");
  if (c.file) {
    const line = el("div", "work-line");
    if (c.language) {
      const dot = el("span", "lang-dot");
      dot.style.background = c.language_color || "#8a8a92";
      line.append(dot, el("span", "work-lang", c.language));
    }
    line.append(el("span", "work-file", c.file));
    if (c.unsaved) {
      const dot = chip("", "warn");
      dot.innerHTML = li("dot");
      dot.title = "Unsaved";
      line.append(dot);
    }
    body.append(line);
  }
  if (p && p.files.length) {
    const peak = Math.max(1, ...p.files.map((f) => f.added + f.deleted));
    const list = el("div", "diff-list");
    for (const f of p.files) {
      const fileRow = el("div", "diff-row");
      fileRow.title = f.path;
      const st = el("span", `diff-st st-${f.status}`);
      st.innerHTML = li({ A: "plus", D: "minus" }[f.status] || "pen");
      st.title = { A: "Added", D: "Deleted" }[f.status] || "Modified";
      fileRow.append(st, el("span", "diff-name", f.name));
      const bar = el("span", "diff-bar");
      const sum = f.added + f.deleted;
      if (sum) {
        bar.style.width = `${Math.max(12, Math.round((sum / peak) * 100))}%`;
        const add = el("i", "add");
        add.style.flex = String(f.added);
        const del = el("i", "del");
        del.style.flex = String(f.deleted);
        bar.append(add, del);
      }
      const barBox = el("span", "diff-barbox");
      barBox.append(bar);
      fileRow.append(barBox, el("span", "diff-num", sum ? `+${f.added} \u2212${f.deleted}` : "\u2014"));
      list.append(fileRow);
    }
    if (p.changed > p.files.length) list.append(el("div", "diff-more", `+${p.changed - p.files.length} more`));
    body.append(list);
  }
  if (body.childElementCount) card.append(body);
  // the editor window for the active project (falls back to any editor window)
  return focusable(collapsible(card, "code"), { exePath: c.exe_path, titleHint: c.project });
}

// ---- downloads: one card per source, up to three downloads on each ----
function formatBytes(n) {
  if (n < 1024) return `${Math.round(n)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

// small line icons: a folder (a browser download opens its folder) and a package (the
// delivery arrived: click to put it away)
const DL_ICON_FOLDER =
  '<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"><path d="M1.8 4.6c0-.7.5-1.2 1.2-1.2h3l1.4 1.6h5.6c.7 0 1.2.5 1.2 1.2v5.4c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z"/></svg>';
const DL_ICON_PACKAGE =
  '<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"><path d="M8 1.6l5.6 2.9v6.9L8 14.3l-5.6-2.9V4.5z"/><path d="M2.6 4.7L8 7.5l5.4-2.8M8 7.5v6.6"/><path d="M5.2 3l5.4 2.8"/></svg>';

function downloadMeta(it) {
  if (it.done) return "";
  const speed = it.speed > 1024 ? ` · ${formatBytes(it.speed)}/s` : "";
  // Steam shows no progress anywhere, so neither does the island: size and bandwidth only
  if (it.kind === "steam") return [it.total ? formatBytes(it.total) : null, speed ? speed.slice(3) : null].filter(Boolean).join(" \u00b7 ") || "Downloading";
  if (it.total) return `${Math.min(100, Math.floor((it.received / it.total) * 100))}%${speed}`;
  return `${formatBytes(it.received)}${speed}`;
}

// ---- LLM sessions (Claude Code): one card per session. Needs you > finished > the others. ----
const LLM_COLOR = { claude: "#e08a5a" };
const LLM_LETTER = { claude: "C" };
const LLM_RING_C = 2 * Math.PI * 18;
const llmCtxColor = (p) => (p > 0.85 ? "#e87a7a" : p > 0.65 ? "#e8b04a" : "#5ac88c");
const LLM_STATE_TIP = { waiting: "Waiting for you", finished: "Finished", working: "Working", idle: "Idle" };
// the state as one icon: a ringing phone, a check, a spinner, a moon
function llmStateIcon(state) {
  const box = el("span", `llm-ico ${state}`);
  box.dataset.tip = LLM_STATE_TIP[state] || state;
  if (state === "working") box.append(el("span", "llm-spin"));
  else box.innerHTML = li({ waiting: "phone", finished: "check", idle: "moon" }[state] || "moon");
  return box;
}
const formatTokens = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : `${Math.round(n / 1000)}k`);

function llmCard(c) {
  const card = el("div", "now-card spot llm-card");
  const row = el("div", "now-row");
  const tile = el("div", "llm-tile");
  tile.style.setProperty("--c", LLM_COLOR[c.provider] || "#888");
  tile.innerHTML = `${LLM_LETTER[c.provider] || "?"}<svg class="cr" viewBox="0 0 40 40"><circle class="rb" cx="20" cy="20" r="18"/><circle class="rf" cx="20" cy="20" r="18"/></svg>`;
  const rf = tile.querySelector(".rf");
  rf.style.strokeDasharray = String(LLM_RING_C);
  const col = el("div", "now-text");
  const title = el("div", "now-title");
  const liveLine = el("div", "llm-live");
  col.append(title, liveLine);
  const right = el("div", "llm-right");
  row.append(tile, col, right);
  card.append(row);

  // opened: the prompt, the latest tool calls, the context window, the numbers
  const body = el("div", "work-body llm-body");
  const prompt = el("p", "llm-prompt");
  const feed = el("div", "llm-feed");
  const ctx = el("div", "llm-ctx");
  const ctxBar = el("i");
  const ctxVal = el("em");
  ctx.append(el("span", "", "Context"), ctxBar, ctxVal);
  const stat = (label) => {
    const box = el("div", "llm-stat");
    const num = el("b");
    box.append(num, el("span", "", label));
    return { box, num };
  };
  const sRun = stat("Running");
  const sCost = stat("Cost");
  const sLines = stat("Lines");
  const sModel = stat("Model");
  const stats = el("div", "llm-stats");
  stats.append(sRun.box, sCost.box, sLines.box, sModel.box);
  const focus = el("button", "llm-focus", "Focus");
  focus.addEventListener("mousedown", (e) => e.stopPropagation());
  body.append(prompt, feed, ctx, stats, focus);
  card.append(body);

  let lastState = "";
  const update = (d) => {
    title.textContent = d.title;
    card.classList.toggle("llm-waiting", d.state === "waiting");
    liveLine.className = `llm-live ${d.state}`;
    liveLine.innerHTML = li(d.activity.icon);
    liveLine.append(el("span", "", d.activity.text));
    // the host (VS Code, a terminal) as a small icon, its name on hover
    if (d.state !== lastState || !right.firstChild) {
      lastState = d.state;
      right.replaceChildren();
      const host = el("span", "llm-host");
      host.innerHTML = li(d.host_icon);
      host.dataset.tip = `${d.host}\n${d.project}`;
      right.append(host, llmStateIcon(d.state));
    }
    const frac = d.context_limit ? Math.min(1, d.context_tokens / d.context_limit) : 0;
    rf.style.stroke = llmCtxColor(frac);
    rf.style.strokeDashoffset = String(LLM_RING_C * (1 - frac));
    prompt.replaceChildren();
    if (d.prompt) {
      prompt.append(el("b", "", "Prompt"), document.createTextNode(d.prompt));
    }
    prompt.hidden = !d.prompt;
    feed.replaceChildren();
    const rows = d.state === "working" ? [d.activity, ...d.feed.slice(0, 2)] : d.feed.slice(0, 3);
    rows.forEach((f, i) => {
      const r = el("div", i === 0 && d.state === "working" ? "llm-fr now" : "llm-fr");
      r.innerHTML = li(f.icon);
      r.append(el("span", "", f.text));
      feed.append(r);
    });
    ctxBar.style.setProperty("--v", `${frac * 100}%`);
    ctxBar.style.setProperty("--c", llmCtxColor(frac));
    ctxVal.textContent = `${formatTokens(d.context_tokens)} / ${formatTokens(d.context_limit)}`;
    sRun.num.textContent = formatDuration(d.running_secs);
    sCost.num.textContent = `$${d.cost_usd.toFixed(2)}`;
    sLines.num.textContent = `+${d.lines_added} \u2212${d.lines_removed}`;
    sModel.num.textContent = d.model || "\u2014";
    focus.hidden = d.state !== "waiting" && d.state !== "finished";
  };
  update(c);
  const args = () => ({ exePath: c.host_exe, titleHint: c.project });
  // to the host window; a finished session has been looked at then, so its card goes
  const go = () => {
    invoke("focus_source", args());
    if (lastState === "finished") {
      invoke("llm_dismiss", { id: c.id }).then(() => refreshActivity());
    }
  };
  focus.addEventListener("click", (e) => {
    e.stopPropagation();
    go();
  });
  card._sig = `llm:${c.id}`;
  card._data = c;
  card._update = update;
  return focusable(collapsible(card, `llm:${c.id}`), args(), go);
}

const DL_RING_C = 2 * Math.PI * 15; // circumference of the progress ring (r = 15 in a 36 box)

// the combined download speed, one sample per refresh (about a second): the last half minute
const dlSpeedHistory = Array.from({ length: 30 }, () => 0);
function sparkSvg() {
  const h = dlSpeedHistory;
  // adaptive: the lowest and highest speed of the window are the bottom and top of the graph, so
  // the line always uses its whole height. A nearly flat stretch is not blown up into noise: the
  // range is at least 10% of the top speed (or 50 KB/s), and sits centred around it.
  const hi = Math.max(...h);
  const lo = Math.min(...h);
  const span = Math.max(hi - lo, hi * 0.1, 50_000);
  const base = hi === 0 ? 0 : hi - lo >= span ? lo : (hi + lo) / 2 - span / 2;
  const pts = h.map((v, i) => [(i / (h.length - 1)) * 120, 25 - Math.max(0, Math.min(1, (v - base) / span)) * 22]);
  const line = pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");
  return `<svg viewBox="0 0 120 28" preserveAspectRatio="none"><path d="${line} L120 28 L0 28 Z" class="a"/><path d="${line}" class="l"/></svg>`;
}

function downloadCard(d) {
  const card = el("div", "now-card spot dl-card");
  // header: a ring of overall progress around the download icon (the count on its corner), a trace
  // of the combined speed over the last half minute, and the speed (a package when all are done)
  const head = el("div", "now-row");
  const ring = el("div", "dl-ring");
  ring.innerHTML = `<svg class="r" viewBox="0 0 36 36"><circle class="rb" cx="18" cy="18" r="15"/><circle class="rf" cx="18" cy="18" r="15"/></svg>${li("download")}<b class="dl-badge"></b>`;
  const ringFill = ring.querySelector(".rf");
  const badge = ring.querySelector(".dl-badge");
  const spark = el("div", "dl-spark");
  const headRight = el("div", "now-total-label dl-total");
  head.append(ring, spark, headRight);
  card.append(head);
  const list = el("div", "dl-list");
  const refs = [];
  for (const it of d.items) {
    const item = el("div", `dl-item ${it.done ? "done" : "focus"}${it.kind === "steam" ? " nobar" : ""}`);
    item.title = it.name;
    const bar = el("i", "dl-fill");
    const track = el("span", "dl-bar");
    track.append(bar);
    const meta = el("span", "dl-meta");
    const name = el("span", "dl-name");
    if (it.icon) {
      const app = el("img", "dl-app");
      app.src = it.icon;
      app.alt = "";
      app.title = it.source;
      name.append(app);
    } else {
      name.append(el("b", "dl-src", it.source));
    }
    name.append(document.createTextNode(it.name));
    item.append(name, meta, track);
    item.addEventListener("mousedown", (e) => e.stopPropagation());
    item.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (it.done) {
        // dismisses it (a browser download also opens its folder)
        await invoke("download_item_click", { id: it.id });
        refreshActivity();
      } else if (it.exe_path) {
        invoke("focus_source", { exePath: it.exe_path }); // bring the app up
      }
    });
    list.append(item);
    refs.push({ bar, meta });
  }
  card.append(list);

  const update = (data) => {
    const live = data.items.filter((it) => !it.done);
    badge.textContent = String(data.items.length);
    // overall: what the items that report a size have got of it; none do (Steam) -> a turning sliver
    const sized = live.filter((it) => it.total);
    const total = sized.reduce((s, it) => s + it.total, 0);
    const got = sized.reduce((s, it) => s + Math.min(it.received, it.total), 0);
    const overall = live.length === 0 ? 100 : total ? (got / total) * 100 : null;
    ring.classList.toggle("busy", overall == null);
    ringFill.style.strokeDashoffset = String(DL_RING_C * (1 - (overall == null ? 0.28 : overall / 100)));
    spark.innerHTML = sparkSvg();
    if (live.length) {
      const speed = live.reduce((s, it) => s + (it.speed || 0), 0);
      headRight.classList.remove("dl-done-icon");
      headRight.textContent = speed > 1024 ? `${formatBytes(speed)}/s` : "";
    } else {
      headRight.classList.add("dl-done-icon");
      headRight.innerHTML = DL_ICON_PACKAGE;
    }
    data.items.forEach((it, i) => {
      const r = refs[i];
      if (!r) return;
      if (it.done) {
        // no words: a folder for a browser download (opens it), a package otherwise
        r.meta.innerHTML = it.kind === "browser" ? DL_ICON_FOLDER : DL_ICON_PACKAGE;
        r.meta.title = it.kind === "browser" ? "Open Folder" : "Put Away";
        r.meta.classList.add("dl-done-icon");
      } else {
        r.meta.textContent = downloadMeta(it);
        r.meta.classList.remove("dl-done-icon");
      }
      const pct = it.done ? 100 : it.total ? Math.min(100, (it.received / it.total) * 100) : null;
      r.bar.classList.toggle("busy", pct == null);
      r.bar.style.width = pct == null ? "" : `${pct}%`;
    });
  };
  update(d);
  card._sig = `dl:${d.items.map((i) => i.id + (i.done ? "!" : "")).join(",")}`;
  card._data = d;
  card._update = update;
  card.addEventListener("mousedown", (e) => e.stopPropagation()); // not "toggle the hub"
  return collapsible(card, "dl");
}

let lastActivitySignature = "";
async function refreshActivity() {
  if (currentView !== "hub" || paneInfo.classList.contains("hidden")) return;
  const a = await invoke("get_activity");
  // what the running games use (empty list: nothing to measure, the counters are let go)
  for (const s of await invoke("get_game_stats", { pids: a.games.map((g) => g.pid) })) gameStats.set(s.pid, s);
  const cards = [];
  // sessions that need you lead, then the finished ones (see llm.rs); the rest follow the downloads
  const llm = a.llm.map(llmCard);
  const leading = llm.filter((c) => c._data.state === "waiting" || c._data.state === "finished");
  cards.push(...leading);
  for (const g of a.games) cards.push(gameCard(g));
  // (before the cards are built: they draw the trace)
  const dlSpeed = a.downloads.reduce((s, d) => s + d.items.reduce((t, it) => t + (it.done ? 0 : it.speed || 0), 0), 0);
  dlSpeedHistory.push(dlSpeed);
  dlSpeedHistory.shift();
  for (const d of a.downloads) cards.push(downloadCard(d));
  cards.push(...llm.filter((c) => !leading.includes(c)));
  if (a.coding) cards.push(codingCard(a.coding));
  for (const w of a.work) cards.push(workCard(w));
  const signature = cards.map((c) => c._sig ?? c.outerHTML).join("");
  if (signature !== lastActivitySignature) {
    lastActivitySignature = signature;
    nowCardsEl.replaceChildren(...cards);
  } else {
    // same cards, new numbers: update in place so a click is never lost to a re-render
    cards.forEach((c, i) => {
      const old = nowCardsEl.children[i];
      if (old && old._update && c._data) old._update(c._data);
    });
  }
  hubHasActivity = cards.length > 0;
  updateInfoEmpty();
}
setInterval(refreshActivity, 1000);


// -- tooltips: a single glass bubble instead of the native Win32 one. Any
// element with data-tip="Headline\nsub line" gets it (hub only -- the small
// pill has no room for one). Ring elements carry data-color for the dot. --
const tipEl = document.createElement("div");
tipEl.id = "tip";
document.body.append(tipEl);
let tipTarget = null;
let tipTimer = 0;

function renderTip(target) {
  const [head, ...rest] = (target.dataset.tip || "").split("\n");
  const h = el("div", "tip-head");
  if (target.dataset.color) {
    const dot = el("span", "tip-dot");
    dot.style.background = target.dataset.color;
    h.append(dot);
  }
  h.append(document.createTextNode(head));
  tipEl.replaceChildren(h);
  if (rest.length) tipEl.append(el("div", "tip-sub", rest.join(" ")));
}

function placeTip(target) {
  const r = target.getBoundingClientRect();
  const tw = tipEl.offsetWidth;
  const th = tipEl.offsetHeight;
  const x = Math.max(8, Math.min(r.left + r.width / 2 - tw / 2, window.innerWidth - tw - 8));
  const below = r.bottom + 10 + th <= window.innerHeight - 4;
  const y = below ? r.bottom + 10 : r.top - th - 10;
  tipEl.style.left = `${x}px`;
  tipEl.style.top = `${y}px`;
  tipEl.style.transformOrigin = `${r.left + r.width / 2 - x}px ${below ? "0" : "100%"}`;
  tipEl.style.setProperty("--tip-shift", below ? "-4px" : "4px");
}

function showTip(target) {
  if (!target.dataset.tip || currentView !== "hub") return;
  tipTarget = target;
  renderTip(target);
  placeTip(target);
  tipEl.classList.add("show");
}

function hideTip() {
  clearTimeout(tipTimer);
  tipTarget = null;
  tipEl.classList.remove("show");
}

function refreshTip(target) {
  if (tipTarget === target) {
    renderTip(target);
    placeTip(target);
  }
}

document.addEventListener("mouseover", (e) => {
  const target = e.target.closest?.("[data-tip]");
  if (target === tipTarget) return;
  hideTip();
  if (target) tipTimer = setTimeout(() => showTip(target), 80);
});
document.addEventListener("mouseleave", hideTip);
document.addEventListener("mousedown", hideTip);
hubBody.addEventListener("scroll", hideTip);

// Every native title="..." (the plain Windows tooltip) becomes the glass tooltip above: its text moves
// to data-tip. Done for what is in the page now and for anything added or retitled later.
function adoptTitle(node) {
  if (node.nodeType !== 1 || !node.hasAttribute("title")) return;
  const text = node.getAttribute("title");
  if (text) node.dataset.tip = text;
  node.removeAttribute("title");
  if (node === tipTarget) refreshTip(node);
}
function adoptTitles(root) {
  adoptTitle(root);
  if (root.querySelectorAll) root.querySelectorAll("[title]").forEach(adoptTitle);
}
adoptTitles(document.body);
new MutationObserver((muts) => {
  for (const m of muts) {
    if (m.type === "attributes") adoptTitle(m.target);
    else m.addedNodes.forEach(adoptTitles);
  }
}).observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ["title"] });
