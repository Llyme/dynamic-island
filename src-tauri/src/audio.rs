//! System-audio "listening" for the eyes: captures the default output device
//! through WASAPI loopback (what you hear -- nothing is recorded or stored),
//! reduces each ~21 ms block to a few numbers, and emits `audio-tick` events:
//! loudness, a beat pulse, and a rough speech-vs-music call. All heuristics on
//! FFT band energies and the energy envelope -- no ML, a couple of percent of
//! one core at most, and it idles (device closed) while the setting is off.
//!
//! Speech tends to have: little bass, an energy envelope that swings hard at
//! syllable rate, and frequent tiny pauses. Music: more low end, steadier
//! (or steadily pulsing) energy, few pauses. Neither is exact, so the call is
//! smoothed with hysteresis.

use crate::IslandState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

const FFT_N: usize = 1024;
const WINDOW: usize = 64; // frames of history for the speech/music call (~1.4 s @ 48 kHz)
const EMIT_EVERY: u32 = 2; // frames per emitted tick (~23 Hz)

#[derive(Serialize, Clone)]
pub struct AudioTick {
    /// 0..1 overall loudness (normalized against the recent peak)
    pub level: f32,
    /// 0..1 per-band energy, each normalized against its own recent peak:
    /// bass <230 Hz, low-mid <800 Hz, mid <3 kHz, high <12 kHz
    pub bass: f32,
    pub lowmid: f32,
    pub mid: f32,
    pub high: f32,
    /// a beat landed since the previous tick (a real onset, or a tempo-predicted
    /// one when the track has a steady pulse but this hit was too soft to catch)
    pub beat: bool,
    /// a bass-drum-style hit (sub-160 Hz onset): unlike `beat` it ignores vocals and melody,
    /// so a head-bang can land exactly on the kick
    pub kick: bool,
    /// 0 = nothing, else how hard a real onset just landed (0..1). Unlike `beat` it has a very
    /// short refractory, so fast hits (hi-hats, rolls) each register -- and only real audio
    /// events count, never a tempo-predicted one. Drives the sound-wave rings.
    pub hit: f32,
    /// "silent" | "speech" | "music"
    pub kind: &'static str,
    /// 0..1 how voice-like the sound is (continuous, so visuals can blend)
    pub voice: f32,
    /// estimated tempo and how sure we are of it (0..1)
    pub bpm: f32,
    pub conf: f32,
    /// -1 (all left) .. 1 (all right): where the sound sits in the stereo field
    pub pan: f32,
    /// 0..1 how energetic the sound is, from absolute loudness (not the
    /// peak-normalized `level`), how sharp/frequent the hits are, and tempo.
    /// Soft ambient or chill music sits low; loud percussive dance music high.
    pub energy: f32,
}

fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    // bit-reversal permutation
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (xr, xi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let (vr, vi) = (xr * cr - xi * ci, xr * ci + xi * cr);
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

const FLUX_HIST: usize = 192; // ~4 s of onset strength for the tempo estimate

struct Analyzer {
    rate: f32,
    hann: Vec<f32>,
    // rolling history for the speech/music call
    energy: Vec<f32>,
    low: Vec<f32>,
    slow_peak: f32,
    speech_ema: f32,
    voice: f32,
    level: f32,
    frame_no: u32,
    // per-band normalization
    band_peak: [f32; 4],
    band_env: [f32; 4],
    // onsets + tempo
    prev_log: Vec<f32>,
    flux_prev: f32,
    prev_log_bass: [f32; 16],
    bass_prev: f32,
    bass_avg: f32,
    kick_since: u32,
    kick_pending: bool,
    hit_since: u32,
    hit_pending: f32,
    flux_avg: f32,
    flux_hist: Vec<f32>,
    onset_since: u32,
    period: f32, // frames per beat
    raw_bpm: f32, // tempo before folding into 70..160 (slow music stays slow)
    conf: f32,
    phase: f32, // 0..1 within the current beat
    beat_pending: bool,
    pan: f32,
    // intensity: how hard the music actually hits, independent of how it is normalized
    db_ema: f32,
    onset_ratio_ema: f32,
    onset_rate_ema: f32, // hits per second
    frames_since_onset: u32,
    intensity: f32,
    /// slow average of the tempo estimate (the raw one wanders by +-20 bpm)
    tempo_ema: f32,
}

impl Analyzer {
    fn new(rate: f32) -> Self {
        let hann = (0..FFT_N)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT_N - 1) as f32).cos())
            .collect();
        Self {
            rate,
            hann,
            energy: Vec::new(),
            low: Vec::new(),
            slow_peak: 0.4,
            speech_ema: 0.0,
            voice: 0.0,
            level: 0.0,
            frame_no: 0,
            band_peak: [1e-3; 4],
            band_env: [0.0; 4],
            prev_log: vec![0.0; FFT_N / 2],
            flux_prev: 0.0,
            prev_log_bass: [0.0; 16],
            bass_prev: 0.0,
            bass_avg: 0.0,
            kick_since: 99,
            kick_pending: false,
            hit_since: 99,
            hit_pending: 0.0,
            flux_avg: 0.0,
            flux_hist: Vec::new(),
            onset_since: 99,
            period: 28.0, // ~100 bpm until we hear better
            raw_bpm: 100.0,
            conf: 0.0,
            phase: 0.0,
            beat_pending: false,
            pan: 0.0,
            db_ema: -80.0,
            onset_ratio_ema: 1.0,
            onset_rate_ema: 0.0,
            frames_since_onset: 99,
            intensity: 0.0,
            tempo_ema: 100.0,
        }
    }

    fn fps(&self) -> f32 {
        self.rate / FFT_N as f32
    }

    fn bin(&self, hz: f32) -> usize {
        ((hz * FFT_N as f32 / self.rate) as usize).clamp(1, FFT_N / 2 - 1)
    }

    /// Autocorrelation of the onset-strength history -> tempo + confidence.
    fn estimate_tempo(&mut self) {
        let n = self.flux_hist.len();
        if n < 96 {
            return;
        }
        let mean = self.flux_hist.iter().sum::<f32>() / n as f32;
        let x: Vec<f32> = self.flux_hist.iter().map(|v| v - mean).collect();
        let r0: f32 = x.iter().map(|v| v * v).sum::<f32>() + 1e-9;
        // lags covering ~30..180 bpm
        let lo = (self.fps() * 60.0 / 180.0) as usize;
        let hi = ((self.fps() * 60.0 / 60.0) as usize).min(n / 2);
        let (mut best_lag, mut best_r) = (0usize, 0.0f32);
        for lag in lo..=hi {
            let r: f32 = (lag..n).map(|i| x[i] * x[i - lag]).sum::<f32>() / (n - lag) as f32;
            let r = r * n as f32 / r0;
            if r > best_r {
                best_r = r;
                best_lag = lag;
            }
        }
        if best_lag == 0 {
            self.conf *= 0.9;
            return;
        }
        // fold into a danceable 70..160 bpm range
        let mut bpm = 60.0 * self.fps() / best_lag as f32;
        self.raw_bpm += (bpm - self.raw_bpm) * 0.35;
        while bpm < 70.0 {
            bpm *= 2.0;
        }
        while bpm > 160.0 {
            bpm /= 2.0;
        }
        let new_period = 60.0 * self.fps() / bpm;
        // glide toward it -- tempo doesn't jump around
        self.period += (new_period - self.period) * 0.35;
        self.conf += (best_r.clamp(0.0, 1.0) - self.conf) * 0.4;
    }

    /// One FFT_N-sample mono block -> maybe a tick to emit.
    fn process(&mut self, block: &[f32], energy_l: f32, energy_r: f32) -> Option<AudioTick> {
        let mut re: Vec<f32> = block.iter().zip(&self.hann).map(|(s, w)| s * w).collect();
        let mut im = vec![0.0f32; FFT_N];
        fft(&mut re, &mut im);
        let mag = |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        let power = |k: usize| re[k] * re[k] + im[k] * im[k];

        let edges = [
            1,
            self.bin(230.0),
            self.bin(800.0),
            self.bin(3000.0),
            self.bin(12000.0),
        ];
        let mut band_pow = [0.0f32; 4];
        for b in 0..4 {
            band_pow[b] = (edges[b]..edges[b + 1]).map(power).sum();
        }
        let low = band_pow[0];
        let total = band_pow.iter().sum::<f32>() + 1e-9;

        // per-band envelopes, each stretched against its own decaying peak so
        // every band uses its full range whatever the track's mix
        for b in 0..4 {
            let nb = (edges[b + 1] - edges[b]).max(1) as f32;
            let e = (band_pow[b] / nb).sqrt();
            self.band_peak[b] = (self.band_peak[b] * 0.9994).max(e).max(1e-3);
            let target = (e / self.band_peak[b]).clamp(0.0, 1.0);
            let k = if target > self.band_env[b] { 0.7 } else { 0.22 };
            self.band_env[b] += (target - self.band_env[b]) * k;
        }

        // loudness: block RMS in dBFS mapped -60..-12 -> 0..1, then stretched
        // against a slowly decaying recent peak so quiet-but-active audio
        // still moves the eyes
        let rms = (block.iter().map(|s| s * s).sum::<f32>() / FFT_N as f32).sqrt();
        let db = 20.0 * (rms + 1e-9).log10();
        let abs_level = ((db + 60.0) / 48.0).clamp(0.0, 1.0);
        // ~1 s average of the real (un-normalized) loudness
        self.db_ema += (db.max(-80.0) - self.db_ema) * 0.025;
        self.slow_peak = (self.slow_peak * 0.9993).max(abs_level).max(0.35);
        let target = (abs_level / self.slow_peak).clamp(0.0, 1.0);
        let k = if target > self.level { 0.6 } else { 0.18 };
        self.level += (target - self.level) * k;

        // stereo position: which side carries more of the energy (fast follow,
        // so a hit panned hard left reads as left right now)
        let (l, r) = (energy_l.sqrt(), energy_r.sqrt());
        let pan_now = if abs_level > 0.08 { (r - l) / (r + l + 1e-6) } else { 0.0 };
        self.pan += (pan_now - self.pan) * 0.5;

        // onset strength: positive change in log-magnitude below ~4 kHz
        let flux_top = self.bin(4000.0);
        let mut flux = 0.0f32;
        for k in 1..flux_top {
            let lm = (1.0 + mag(k)).ln();
            flux += (lm - self.prev_log[k]).max(0.0);
            self.prev_log[k] = lm;
        }
        // low-end onset (kick drum): the same log-magnitude flux, but only the
        // bins under ~160 Hz where vocals barely reach
        let kick_top = self.bin(160.0).clamp(3, 15);
        let mut bflux = 0.0f32;
        for k in 1..kick_top {
            let lm = (1.0 + mag(k)).ln();
            bflux += (lm - self.prev_log_bass[k]).max(0.0);
            self.prev_log_bass[k] = lm;
        }
        self.kick_since = self.kick_since.saturating_add(1);
        if abs_level > 0.2 && bflux > self.bass_avg * 1.7 + 0.35 && bflux > self.bass_prev && self.kick_since > 8 {
            self.kick_pending = true;
            self.kick_since = 0;
        }
        self.bass_avg += (bflux - self.bass_avg) * 0.06;
        self.bass_prev = bflux;
        self.flux_hist.push(flux);
        if self.flux_hist.len() > FLUX_HIST {
            self.flux_hist.remove(0);
        }
        self.onset_since = self.onset_since.saturating_add(1);
        let thr = self.flux_avg * 1.45 + 0.4;
        // every genuine onset, however fast (min ~90 ms apart), with its strength
        self.hit_since = self.hit_since.saturating_add(1);
        if abs_level > 0.2 && flux > thr && flux > self.flux_prev && self.hit_since > 4 {
            let strength = (((flux / (self.flux_avg + 0.4)) - 1.2) / 3.0).clamp(0.0, 1.0);
            self.hit_pending = self.hit_pending.max(strength);
            self.hit_since = 0;
        }
        // a hit can ring for several frames -- one beat per hit, never a burst
        let refractory = ((self.period * 0.5) as u32).max(11);
        if abs_level > 0.2 && flux > thr && flux > self.flux_prev && self.onset_since > refractory {
            // a real hit: report it, and pull the beat clock onto it
            self.beat_pending = true;
            self.onset_since = 0;
            // how sharp was it (vs. the recent baseline), and how often do they come
            let ratio = flux / (self.flux_avg + 0.4);
            self.onset_ratio_ema += (ratio - self.onset_ratio_ema) * 0.25;
            let rate = self.fps() / (self.frames_since_onset.max(1) as f32);
            self.onset_rate_ema += (rate.min(6.0) - self.onset_rate_ema) * 0.2;
            self.frames_since_onset = 0;
            let err = if self.phase > 0.5 { self.phase - 1.0 } else { self.phase };
            self.phase -= err * 0.4;
        }
        self.flux_avg += (flux - self.flux_avg) * 0.05;
        // no hits for a while -> the sharpness/rate estimates relax back down
        self.frames_since_onset = self.frames_since_onset.saturating_add(1);
        self.onset_ratio_ema += (1.0 - self.onset_ratio_ema) * 0.004;
        self.onset_rate_ema -= self.onset_rate_ema * 0.006;
        self.flux_prev = flux;

        // the beat clock free-runs at the estimated tempo; when it wraps and no
        // real onset came near it, that's a beat too (soft hit / dropped frame)
        self.phase += 1.0 / self.period.max(8.0);
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            if self.conf > 0.3 && abs_level > 0.2 && self.onset_since as f32 > self.period * 0.75 {
                self.beat_pending = true;
            }
        }
        if self.frame_no % 24 == 0 {
            self.estimate_tempo();
        }

        // speech/music history
        self.energy.push(total);
        self.low.push(low);
        if self.energy.len() > WINDOW {
            self.energy.remove(0);
            self.low.remove(0);
        }
        if self.energy.len() == WINDOW {
            let n = WINDOW as f32;
            let mean = self.energy.iter().sum::<f32>() / n;
            let var = self.energy.iter().map(|e| (e - mean) * (e - mean)).sum::<f32>() / n;
            let cv = var.sqrt() / (mean + 1e-9);
            let pause_frac = self.energy.iter().filter(|&&e| e < 0.2 * mean).count() as f32 / n;
            let low_ratio = self.low.iter().sum::<f32>() / (self.energy.iter().sum::<f32>() + 1e-9);
            let score = 0.45 * ((cv - 0.45) / 0.5).clamp(0.0, 1.0)
                + 0.25 * ((0.30 - low_ratio) / 0.25).clamp(0.0, 1.0)
                + 0.30 * (pause_frac / 0.15).clamp(0.0, 1.0);
            // a steady pulse is a strong sign of music, not speech
            let score = score * (1.0 - 0.75 * self.conf);
            self.speech_ema += (score - self.speech_ema) * 0.05;
            let voice_target = ((self.speech_ema - 0.36) / 0.35).clamp(0.0, 1.0);
            self.voice += (voice_target - self.voice) * 0.08;
        }

        let _bpm_now = 60.0 * self.fps() / self.period;
        // How energetic it sounds. Loudness is the weakest signal (system volume
        // and mastering make it unreliable); the pulse -- how often hits come,
        // how fast the tempo is, how sharp the hits are -- carries the call.
        self.tempo_ema += (self.raw_bpm - self.tempo_ema) * 0.01;
        let abs_norm = ((self.db_ema + 45.0) / 30.0).clamp(0.0, 1.0);
        let punch = ((self.onset_ratio_ema - 1.6) / 2.4).clamp(0.0, 1.0);
        let rate_f = ((self.onset_rate_ema - 0.8) / 1.2).clamp(0.0, 1.0);
        let tempo_f = if self.conf > 0.25 { ((self.tempo_ema - 70.0) / 35.0).clamp(0.0, 1.0) } else { 0.4 };
        let energy_target = if self.level < 0.05 {
            0.0
        } else {
            0.1 * abs_norm + 0.2 * punch + 0.4 * rate_f + 0.3 * tempo_f
        };
        #[cfg(test)]
        if self.frame_no % 60 == 0 {
            println!("  db={:.1} abs={abs_norm:.2} punch={punch:.2} (ratio {:.2}) rate={rate_f:.2} ({:.2}/s) tempo={tempo_f:.2} bpm={_bpm_now:.0} conf={:.2}", self.db_ema, self.onset_ratio_ema, self.onset_rate_ema, self.conf);
        }
        #[cfg(debug_assertions)]
        if self.frame_no % 60 == 0 && std::env::var_os("DI_AUDIO_LOG").is_some() {
            eprintln!("comp: db={:.1} abs={abs_norm:.2} punch={punch:.2} (ratio {:.2}) rate={rate_f:.2} ({:.2}/s) tempo={tempo_f:.2} bpm={_bpm_now:.0} conf={:.2} speech_ema={:.2} voice={:.2} target={energy_target:.2}", self.db_ema, self.onset_ratio_ema, self.onset_rate_ema, self.conf, self.speech_ema, self.voice);
        }
        self.intensity += (energy_target - self.intensity) * 0.03;

        self.frame_no += 1;
        if self.frame_no % EMIT_EVERY != 0 {
            return None;
        }
        let kind = if self.level < 0.05 {
            "silent"
        } else if self.voice > 0.6 {
            "speech"
        } else {
            "music"
        };
        Some(AudioTick {
            level: self.level,
            bass: self.band_env[0],
            lowmid: self.band_env[1],
            mid: self.band_env[2],
            high: self.band_env[3],
            beat: std::mem::take(&mut self.beat_pending),
            kick: std::mem::take(&mut self.kick_pending),
            hit: std::mem::take(&mut self.hit_pending),
            kind,
            voice: self.voice,
            bpm: 60.0 * self.fps() / self.period,
            conf: self.conf,
            pan: self.pan,
            energy: self.intensity,
        })
    }
}

/// Runs one capture session on the current default output device. Returns when
/// the device disappears/changes, or the setting is switched off.
unsafe fn capture_session(app: &AppHandle, state: &Arc<IslandState>) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let pwfx = client.GetMixFormat()?;
    let channels = std::ptr::addr_of!((*pwfx).nChannels).read_unaligned() as usize;
    let rate = std::ptr::addr_of!((*pwfx).nSamplesPerSec).read_unaligned() as f32;
    let bits = std::ptr::addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 1_000_000, 0, pwfx, None)?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;

    let mut analyzer = Analyzer::new(rate);
    let mut pending: Vec<f32> = Vec::with_capacity(FFT_N * 2);
    let mut pending_lr: Vec<(f32, f32)> = Vec::with_capacity(FFT_N * 2);
    let mut was_silent_emitted = false;
    let mut idle_polls = 0u32;

    let result = (|| -> windows::core::Result<()> {
        loop {
            if !state.audio_enabled.load(Ordering::Relaxed) {
                return Ok(());
            }
            // nothing has played for ~1 s: poll lazily (the analyzer has decayed to silence)
            std::thread::sleep(Duration::from_millis(if idle_polls >= 60 { 80 } else { 15 }));

            let mut got_data = false;
            let mut packet = capture.GetNextPacketSize()?;
            while packet != 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                got_data = true;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                for f in 0..frames as usize {
                    let mut acc = 0.0f32;
                    let (mut left, mut right) = (0.0f32, 0.0f32);
                    if !silent {
                        for c in 0..channels {
                            let idx = f * channels + c;
                            let v = if bits == 32 {
                                (data as *const f32).add(idx).read_unaligned()
                            } else {
                                (data as *const i16).add(idx).read_unaligned() as f32 / 32768.0
                            };
                            acc += v;
                            // front-left / front-right are the first two channels
                            // (mono: both sides get the same signal)
                            if c == 0 {
                                left = v;
                                if channels == 1 {
                                    right = v;
                                }
                            } else if c == 1 {
                                right = v;
                            }
                        }
                    }
                    pending.push(acc / channels.max(1) as f32);
                    pending_lr.push((left * left, right * right));
                }
                capture.ReleaseBuffer(frames)?;
                packet = capture.GetNextPacketSize()?;
            }

            // loopback delivers nothing at all while nothing plays -- feed zeros
            // so the analyzer decays to silence
            if !got_data {
                idle_polls = idle_polls.saturating_add(1);
                if idle_polls > 4 && idle_polls < 60 {
                    pending.extend(std::iter::repeat(0.0).take(FFT_N / 2));
                    pending_lr.extend(std::iter::repeat((0.0, 0.0)).take(FFT_N / 2));
                }
            } else {
                idle_polls = 0;
            }

            while pending.len() >= FFT_N {
                let block: Vec<f32> = pending.drain(..FFT_N).collect();
                let (el, er) = pending_lr
                    .drain(..FFT_N)
                    .fold((0.0f32, 0.0f32), |(a, b), (l, r)| (a + l, b + r));
                if let Some(tick) = analyzer.process(&block, el, er) {
                    let silent = tick.kind == "silent";
                    // don't spam identical silence
                    if !(silent && was_silent_emitted) {
                        #[cfg(debug_assertions)]
                        if std::env::var_os("DI_AUDIO_LOG").is_some() {
                            eprintln!("audio: {} level={:.2} bass={:.2} voice={:.2} bpm={:.0} conf={:.2} pan={:.2} energy={:.2} beat={} kick={}", tick.kind, tick.level, tick.bass, tick.voice, tick.bpm, tick.conf, tick.pan, tick.energy, tick.beat, tick.kick);
                        }
                        let _ = app.emit("audio-tick", tick);
                    }
                    was_silent_emitted = silent;
                }
            }
        }
    })();

    let _ = client.Stop();
    result
}

pub fn spawn(app: AppHandle, state: Arc<IslandState>) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        loop {
            if !state.audio_enabled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            // returns on device change/removal (or the toggle) -- just retry
            let _ = unsafe { capture_session(&app, &state) };
            std::thread::sleep(Duration::from_millis(1000));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(gen: impl Fn(usize) -> f32, seconds: f32) -> (String, usize, f32) {
        let rate = 48000.0;
        let mut a = Analyzer::new(rate);
        let total = (rate * seconds) as usize;
        let mut kinds: Vec<&str> = Vec::new();
        let mut beats = 0;
        let mut level = 0.0;
        let mut i = 0;
        while i + FFT_N <= total {
            let block: Vec<f32> = (i..i + FFT_N).map(&gen).collect();
            if let Some(t) = a.process(&block, 0.0, 0.0) {
                kinds.push(t.kind);
                if t.beat {
                    beats += 1;
                }
                level = t.level;
            }
            i += FFT_N;
        }
        let last = kinds.iter().rev().take(20).cloned().collect::<Vec<_>>();
        let speech = last.iter().filter(|k| **k == "speech").count();
        let music = last.iter().filter(|k| **k == "music").count();
        (format!("speech={speech} music={music}"), beats, level)
    }

    #[test]
    fn classifies_synthetic_signals() {
        let rate = 48000.0f32;
        // speech-ish: 300 Hz + 1.2 kHz formants, ~4 Hz syllable envelope with pauses
        let speech = |n: usize| {
            let t = n as f32 / rate;
            let syl = ((t * 4.0 * std::f32::consts::TAU).sin()).max(0.0).powi(2);
            let gate = if (t * 0.9).fract() > 0.75 { 0.0 } else { 1.0 };
            0.25 * syl * gate
                * ((t * 300.0 * std::f32::consts::TAU).sin() + 0.6 * (t * 1200.0 * std::f32::consts::TAU).sin())
        };
        // music-ish: steady chords + 2 Hz kick (60 Hz) pulses
        let music = |n: usize| {
            let t = n as f32 / rate;
            let kick = (-((t * 2.0).fract()) * 12.0).exp() * (t * 60.0 * std::f32::consts::TAU).sin();
            0.2 * kick
                + 0.12 * (t * 440.0 * std::f32::consts::TAU).sin()
                + 0.1 * (t * 554.0 * std::f32::consts::TAU).sin()
                + 0.08 * (t * 659.0 * std::f32::consts::TAU).sin()
        };
        println!("speech-like: {:?}", run(speech, 6.0));
        println!("music-like:  {:?}", run(music, 6.0));
        println!("silence:     {:?}", run(|_| 0.0, 3.0));

        // chill: quiet slow pad + a soft, sparse pulse at ~70 bpm
        let chill = |n: usize| {
            let t = n as f32 / rate;
            let pulse = (-((t * 1.15).fract()) * 6.0).exp() * (t * 90.0 * std::f32::consts::TAU).sin();
            0.03 * pulse
                + 0.035 * (t * 220.0 * std::f32::consts::TAU).sin()
                + 0.03 * (t * 277.0 * std::f32::consts::TAU).sin()
                + 0.025 * (t * 330.0 * std::f32::consts::TAU).sin()
        };
        // disco: loud four-on-the-floor at 124 bpm with open hats and a bassline
        let disco = |n: usize| {
            let t = n as f32 / rate;
            let beat = t * 124.0 / 60.0;
            let kick = (-(beat.fract()) * 14.0).exp() * (t * 55.0 * std::f32::consts::TAU).sin();
            let hat = (-(((beat + 0.5).fract()) * 30.0)).exp() * (t * 7000.0 * std::f32::consts::TAU).sin();
            0.45 * kick + 0.12 * hat + 0.12 * (t * 110.0 * std::f32::consts::TAU).sin() * (1.0 + (beat * 2.0).sin())
                + 0.08 * (t * 660.0 * std::f32::consts::TAU).sin()
        };
        let energy_of = |gen: &dyn Fn(usize) -> f32| {
            let mut a = Analyzer::new(rate);
            let mut last = 0.0;
            let mut i = 0;
            while i + FFT_N <= (rate * 8.0) as usize {
                let block: Vec<f32> = (i..i + FFT_N).map(gen).collect();
                if let Some(t) = a.process(&block, 0.0, 0.0) {
                    last = t.energy;
                }
                i += FFT_N;
            }
            last
        };
        let (ec, ed) = (energy_of(&chill), energy_of(&disco));
        println!("energy chill={ec:.2} disco={ed:.2}");
        assert!(ec < 0.4, "chill should read low, got {ec}");
        assert!(ed > 0.6, "disco should read high, got {ed}");
    }
}
