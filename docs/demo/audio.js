// A small synth that plays three original loops in the browser, and reports what it plays the way the app's
// WASAPI loopback analyzer does (audio.rs): an `audio-tick` ~23 times a second with loudness, four band levels,
// beat / kick / hit onsets, tempo, pan and energy. The island's frontend reads those exactly as it reads the real
// ones, so the eyes, the sound light and the rings react to music that is really playing.
(() => {
  "use strict";
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
  const mtof = (m) => 440 * Math.pow(2, (m - 69) / 12);

  const TRACKS = [
    { title: "Midnight Drive", artist: "The Parallels", dur: 187, bpm: 104, energy: 0.5, hue: [262, 330], vibe: "groove" },
    { title: "Neon Rush", artist: "Kira Vale", dur: 214, bpm: 138, energy: 0.86, hue: [320, 195], vibe: "bang" },
    { title: "Slow Tide", artist: "Haru Mori", dur: 243, bpm: 68, energy: 0.12, hue: [190, 150], vibe: "chill" },
  ];

  // chord roots (midi) per bar, A minor-ish: Am F C G
  const PROG = [57, 53, 60, 55];
  const MINOR = [0, 3, 7, 10];
  const MAJOR = [0, 4, 7, 11];

  let ctx = null;
  let master = null;
  let tap = null; // analyser (before the volume, so the island reacts even when muted)
  let out = null;
  let noiseBuf = null;
  let muted = false;
  let playing = false;
  let trackIdx = 0;
  let rate = 1;
  let step = 0;
  let nextTime = 0;
  let schedTimer = 0;
  let tickTimer = 0;
  const events = []; // { t, kind, vel, pan }
  const peaks = { level: 0.05, b: [0.05, 0.05, 0.05, 0.05] };
  let lastPan = 0;
  let timeData = null;
  let freqData = null;

  function ensure() {
    if (ctx) return true;
    const AC = window.AudioContext || window.webkitAudioContext;
    if (!AC) return false;
    ctx = new AC();
    tap = ctx.createAnalyser();
    tap.fftSize = 2048;
    tap.smoothingTimeConstant = 0.5;
    master = ctx.createGain();
    master.gain.value = muted ? 0 : 0.8;
    const comp = ctx.createDynamicsCompressor();
    out = ctx.createGain();
    out.connect(comp);
    comp.connect(tap);
    tap.connect(master);
    master.connect(ctx.destination);
    noiseBuf = ctx.createBuffer(1, ctx.sampleRate, ctx.sampleRate);
    const d = noiseBuf.getChannelData(0);
    for (let i = 0; i < d.length; i++) d[i] = Math.random() * 2 - 1;
    timeData = new Float32Array(tap.fftSize);
    freqData = new Float32Array(tap.frequencyBinCount);
    return true;
  }

  // ---- voices
  function panNode(p) {
    if (ctx.createStereoPanner) {
      const n = ctx.createStereoPanner();
      n.pan.value = p;
      n.connect(out);
      return n;
    }
    return out;
  }
  function env(g, t, a, d, peak, end = 0.0001) {
    g.gain.setValueAtTime(0.0001, t);
    g.gain.exponentialRampToValueAtTime(peak, t + a);
    g.gain.exponentialRampToValueAtTime(end, t + a + d);
  }
  function kick(t, vel = 1) {
    const o = ctx.createOscillator();
    const g = ctx.createGain();
    o.frequency.setValueAtTime(150, t);
    o.frequency.exponentialRampToValueAtTime(42, t + 0.12);
    env(g, t, 0.004, 0.32, 0.95 * vel);
    o.connect(g);
    g.connect(out);
    o.start(t);
    o.stop(t + 0.4);
    events.push({ t, kind: "kick", vel, pan: 0 });
  }
  function noise(t, dur, type, freq, peak, pan = 0, q = 0.7) {
    const s = ctx.createBufferSource();
    s.buffer = noiseBuf;
    const f = ctx.createBiquadFilter();
    f.type = type;
    f.frequency.value = freq;
    f.Q.value = q;
    const g = ctx.createGain();
    env(g, t, 0.002, dur, peak);
    s.connect(f);
    f.connect(g);
    g.connect(panNode(pan));
    s.start(t);
    s.stop(t + dur + 0.05);
  }
  function snare(t, vel = 1) {
    noise(t, 0.18, "bandpass", 1900, 0.5 * vel, 0.05, 0.8);
    const o = ctx.createOscillator();
    const g = ctx.createGain();
    o.type = "triangle";
    o.frequency.setValueAtTime(220, t);
    o.frequency.exponentialRampToValueAtTime(140, t + 0.08);
    env(g, t, 0.002, 0.12, 0.4 * vel);
    o.connect(g);
    g.connect(out);
    o.start(t);
    o.stop(t + 0.2);
    events.push({ t, kind: "snare", vel, pan: 0.05 });
  }
  function hat(t, vel = 0.5, open = false, pan = 0.25) {
    noise(t, open ? 0.16 : 0.05, "highpass", 7500, 0.22 * vel, pan, 0.5);
    events.push({ t, kind: "hat", vel: vel * 0.55, pan });
  }
  function bass(t, midi, dur, type = "sawtooth", vel = 0.45) {
    const o = ctx.createOscillator();
    const f = ctx.createBiquadFilter();
    const g = ctx.createGain();
    o.type = type;
    o.frequency.value = mtof(midi);
    f.type = "lowpass";
    f.frequency.setValueAtTime(900, t);
    f.frequency.exponentialRampToValueAtTime(180, t + dur);
    env(g, t, 0.01, dur, vel);
    o.connect(f);
    f.connect(g);
    g.connect(out);
    o.start(t);
    o.stop(t + dur + 0.05);
  }
  function pad(t, midis, dur, vel = 0.1) {
    for (const m of midis) {
      for (const det of [-6, 6]) {
        const o = ctx.createOscillator();
        const f = ctx.createBiquadFilter();
        const g = ctx.createGain();
        o.type = "sawtooth";
        o.frequency.value = mtof(m);
        o.detune.value = det;
        f.type = "lowpass";
        f.frequency.value = 1400;
        g.gain.setValueAtTime(0.0001, t);
        g.gain.linearRampToValueAtTime(vel, t + dur * 0.35);
        g.gain.linearRampToValueAtTime(0.0001, t + dur);
        o.connect(f);
        f.connect(g);
        g.connect(out);
        o.start(t);
        o.stop(t + dur + 0.1);
      }
    }
  }
  function pluck(t, midi, dur = 0.22, pan = 0, vel = 0.18, type = "triangle") {
    const o = ctx.createOscillator();
    const g = ctx.createGain();
    o.type = type;
    o.frequency.value = mtof(midi);
    env(g, t, 0.004, dur, vel);
    o.connect(g);
    g.connect(panNode(pan));
    o.start(t);
    o.stop(t + dur + 0.05);
    events.push({ t, kind: "note", vel: vel * 2.2, pan });
  }

  // ---- the three loops, one 16th-note step at a time
  function schedule(tr, s, t) {
    const bar = Math.floor(s / 16) % 4;
    const i = s % 16;
    const root = PROG[bar];
    const chord = (bar === 1 || bar === 2 ? MAJOR : MINOR).map((x) => root + x);
    if (tr.vibe === "groove") {
      if ([0, 7, 10].includes(i)) kick(t, i === 0 ? 1 : 0.8);
      if (i === 4 || i === 12) snare(t, 0.9);
      if (i % 2 === 0) hat(t, i % 4 === 0 ? 0.7 : 0.4, false, i % 4 === 0 ? -0.2 : 0.3);
      if ([0, 3, 6, 8, 10, 14].includes(i)) bass(t, root - 12 + (i === 6 || i === 14 ? 7 : 0), 0.22);
      if (i === 0) pad(t, chord.map((m) => m + 12), 60 / tr.bpm / rate * 4, 0.07);
      if (i % 4 === 2) pluck(t, chord[(i / 2) % 3] + 24, 0.2, Math.sin(s * 0.7) * 0.5, 0.1);
    } else if (tr.vibe === "bang") {
      if (i % 4 === 0) kick(t, 1);
      if (i === 4 || i === 12) snare(t, 1);
      if (i % 4 === 2) hat(t, 0.8, true, i % 8 === 2 ? -0.35 : 0.35);
      if (i % 2 === 1 && i % 4 !== 3) hat(t, 0.3, false, 0.15);
      bass(t, root - 12 + (i % 8 === 6 ? 12 : 0), 0.11, "sawtooth", 0.4);
      pluck(t, chord[i % 4] + 24 + (i >= 8 ? 12 : 0), 0.13, Math.sin(s * 0.45) * 0.7, 0.13, "square");
      if (i === 0) pad(t, chord.map((m) => m + 12), 60 / tr.bpm / rate * 4, 0.05);
    } else {
      // chill: long pads, a soft low kick every other bar, sparse bells
      if (i === 0 && s % 32 === 0) kick(t, 0.28);
      if (i === 0) pad(t, chord, 60 / tr.bpm / rate * 4.4, 0.12);
      if (i === 0) bass(t, root - 24, 60 / tr.bpm / rate * 3.6, "sine", 0.5);
      if ([2, 7, 11].includes(i) && Math.random() < 0.7) pluck(t, [72, 74, 76, 79, 81, 84][Math.floor(Math.random() * 6)], 0.9, Math.random() * 1.2 - 0.6, 0.12, "sine");
    }
  }

  function scheduler() {
    const tr = TRACKS[trackIdx];
    const stepLen = 60 / (tr.bpm * clamp(rate, 0.25, 3)) / 4;
    while (nextTime < ctx.currentTime + 0.14) {
      schedule(tr, step, nextTime);
      nextTime += stepLen;
      step++;
    }
  }

  // ---- what the island hears
  function norm(v, key, i) {
    const p = i === undefined ? peaks[key] : peaks.b[i];
    const np = Math.max(v, p * 0.995, 0.02);
    if (i === undefined) peaks[key] = np;
    else peaks.b[i] = np;
    return clamp(v / np, 0, 1);
  }

  function emitTick() {
    if (!ctx || !playing || ctx.state !== "running" || !NADI.settings().react_to_audio) return;
    const tr = TRACKS[trackIdx];
    tap.getFloatTimeDomainData(timeData);
    let sum = 0;
    for (let i = 0; i < timeData.length; i++) sum += timeData[i] * timeData[i];
    const rms = Math.sqrt(sum / timeData.length);
    tap.getFloatFrequencyData(freqData);
    const hz = ctx.sampleRate / tap.fftSize;
    const band = (lo, hi) => {
      let m = 0;
      let n = 0;
      for (let b = Math.max(1, Math.floor(lo / hz)); b <= Math.min(freqData.length - 1, Math.ceil(hi / hz)); b++) {
        m += Math.pow(10, freqData[b] / 20);
        n++;
      }
      return n ? m / n : 0;
    };
    const raw = [band(30, 230), band(230, 800), band(800, 3000), band(3000, 12000)];
    // events whose time has come since the last tick
    const t = ctx.currentTime;
    let beat = false;
    let kickHit = false;
    let hit = 0;
    let pan = lastPan * 0.8;
    while (events.length && events[0].t <= t) {
      const e = events.shift();
      if (t - e.t > 0.25) continue;
      if (e.kind === "kick" || e.kind === "snare") beat = true;
      if (e.kind === "kick") kickHit = true;
      hit = Math.max(hit, Math.min(1, e.vel));
      pan = e.pan * 0.9 + pan * 0.1;
    }
    lastPan = pan;
    const energy = clamp(tr.energy + (beat ? 0.04 : 0) + Math.sin(t * 0.3) * 0.03, 0, 1);
    NADI.emit("audio-tick", {
      level: norm(rms, "level"),
      bass: norm(raw[0], "b", 0),
      lowmid: norm(raw[1], "b", 1),
      mid: norm(raw[2], "b", 2),
      high: norm(raw[3], "b", 3),
      beat,
      kick: kickHit,
      hit,
      kind: "music",
      voice: 0.03,
      bpm: tr.bpm,
      conf: 0.9,
      pan,
      energy,
    });
  }

  const Audio = (window.NADIAudio = {
    TRACKS,
    available: () => !!(window.AudioContext || window.webkitAudioContext),
    get muted() {
      return muted;
    },
    setMuted(m) {
      muted = m;
      if (master) master.gain.value = m ? 0 : 0.8;
    },
    // must run inside a click: browsers only let audio start from one
    play(idx, r = 1) {
      trackIdx = idx;
      rate = r;
      if (!ensure()) return false;
      ctx.resume?.();
      if (!playing) {
        playing = true;
        step = 0;
        nextTime = ctx.currentTime + 0.06;
        events.length = 0;
        schedTimer = setInterval(scheduler, 25);
        tickTimer = setInterval(emitTick, 43);
      }
      return true;
    },
    setTrack(idx) {
      trackIdx = idx;
      if (playing) {
        step = 0;
        nextTime = ctx.currentTime + 0.06;
        events.length = 0;
      }
    },
    setRate(r) {
      rate = r;
    },
    pause() {
      playing = false;
      clearInterval(schedTimer);
      clearInterval(tickTimer);
      events.length = 0;
      peaks.level = 0.05;
      peaks.b = [0.05, 0.05, 0.05, 0.05];
    },
    get playing() {
      return playing;
    },
  });
})();
