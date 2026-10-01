//! Frames per second of the games, from the "Present" events Windows emits for every swap-chain
//! present (DXGI: DirectX 11 and 12 games) -- the way PresentMon counts frames. An ETW trace session
//! is started only while the hub shows a game card (calls to `touch` keep it alive) and stopped a few
//! seconds after the last call. Reading real-time ETW needs administrator rights or membership of the
//! "Performance Log Users" group; without them `denied()` is true and there are simply no FPS numbers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

const SESSION: &str = "NADI-FPS";
/// Microsoft-Windows-DXGI
const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
/// DXGI "Present::Start"
const PRESENT_START: u16 = 42;
/// how long the trace keeps running after the last `touch`
const IDLE_STOP: Duration = Duration::from_secs(6);
/// after a refusal, wait this long before asking again
const RETRY_DENIED: Duration = Duration::from_secs(60);
const KEEP_100NS: i64 = 12 * 10_000_000;

const STOPPED: u8 = 0;
const RUNNING: u8 = 1;
const DENIED: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(STOPPED);

#[derive(Default)]
struct Inner {
    wanted: HashSet<u32>,
    /// present timestamps (100 ns, system time) per wanted pid
    frames: HashMap<u32, VecDeque<i64>>,
    last_touch: Option<Instant>,
    denied_at: Option<Instant>,
    starting: bool,
}

fn inner() -> &'static Mutex<Inner> {
    static I: OnceLock<Mutex<Inner>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(Inner::default()))
}

fn now_100ns() -> i64 {
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    (((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64) as i64
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    if rec.is_null() {
        return;
    }
    let h = &(*rec).EventHeader;
    if h.EventDescriptor.Id != PRESENT_START {
        return;
    }
    let Ok(mut g) = inner().try_lock() else { return };
    let pid = h.ProcessId;
    if !g.wanted.contains(&pid) {
        return;
    }
    let q = g.frames.entry(pid).or_default();
    q.push_back(h.TimeStamp);
    while q.front().map_or(false, |t| h.TimeStamp - *t > KEEP_100NS) {
        q.pop_front();
    }
}

/// a properties block for the session: the struct followed by the session's name
fn properties() -> (Vec<u64>, Vec<u16>) {
    let name: Vec<u16> = SESSION.encode_utf16().chain(std::iter::once(0)).collect();
    let head = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = head + name.len() * 2;
    let mut buf = vec![0u64; total.div_ceil(8)];
    unsafe {
        let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*p).Wnode.BufferSize = total as u32;
        (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*p).Wnode.ClientContext = 2; // timestamps as system time (100 ns)
        (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*p).FlushTimer = 1;
        (*p).LoggerNameOffset = head as u32;
        let dst = (buf.as_mut_ptr() as *mut u8).add(head) as *mut u16;
        std::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
    }
    (buf, name)
}

fn stop_session() {
    let (mut buf, name) = properties();
    unsafe {
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

/// start the session and block, delivering events, until it is stopped
fn run_session() {
    stop_session(); // a leftover from a crashed run would make the start fail
    let (mut buf, name) = properties();
    unsafe {
        let mut handle = CONTROLTRACE_HANDLE::default();
        let r = StartTraceW(&mut handle, PCWSTR(name.as_ptr()), buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES);
        if r != ERROR_SUCCESS {
            let mut g = inner().lock().unwrap();
            g.starting = false;
            g.denied_at = Some(Instant::now());
            STATE.store(DENIED, Ordering::Relaxed);
            return;
        }
        let r = EnableTraceEx2(handle, &DXGI, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, TRACE_LEVEL_VERBOSE as u8, 0, 0, 0, None);
        let mut log: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
        log.LoggerName = PWSTR(name.as_ptr() as *mut u16);
        log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        let h = OpenTraceW(&mut log);
        if r != ERROR_SUCCESS || h.Value == u64::MAX {
            stop_session();
            let mut g = inner().lock().unwrap();
            g.starting = false;
            g.denied_at = Some(Instant::now());
            STATE.store(DENIED, Ordering::Relaxed);
            return;
        }
        STATE.store(RUNNING, Ordering::Relaxed);
        inner().lock().unwrap().starting = false;
        let _ = ProcessTrace(&[h], None, None); // returns when the session is stopped
        let _ = CloseTrace(h);
    }
    STATE.store(STOPPED, Ordering::Relaxed);
    inner().lock().unwrap().frames.clear();
}

/// Tell the counter which games to count, starting the trace if it is not running. Call about once
/// a second while the numbers are shown; an empty list (or no call for a few seconds) stops it.
pub fn touch(pids: &[u32]) {
    let mut g = inner().lock().unwrap();
    if pids.is_empty() {
        g.wanted.clear();
        g.last_touch = None;
        drop(g);
        if STATE.load(Ordering::Relaxed) == RUNNING {
            stop_session();
        }
        return;
    }
    g.wanted = pids.iter().copied().collect();
    g.frames.retain(|p, _| pids.contains(p));
    g.last_touch = Some(Instant::now());
    let state = STATE.load(Ordering::Relaxed);
    let may_start = state == STOPPED
        || (state == DENIED && g.denied_at.map_or(true, |t| t.elapsed() >= RETRY_DENIED));
    if may_start && !g.starting {
        g.starting = true;
        STATE.store(STOPPED, Ordering::Relaxed);
        drop(g);
        std::thread::spawn(|| {
            // a watchdog: the trace costs a little, so it must not outlive the card on screen
            std::thread::spawn(|| loop {
                std::thread::sleep(Duration::from_secs(2));
                let idle = inner().lock().unwrap().last_touch.map_or(true, |t| t.elapsed() > IDLE_STOP);
                match STATE.load(Ordering::Relaxed) {
                    RUNNING if idle => {
                        stop_session();
                        return;
                    }
                    RUNNING => {}
                    _ => return,
                }
            });
            run_session();
        });
    }
}

/// true when Windows refused to start the trace (no rights)
pub fn denied() -> bool {
    STATE.load(Ordering::Relaxed) == DENIED
}

pub struct Sample {
    pub fps: f64,
    /// FPS of the slowest 1% of frames over the last seconds
    pub low: f64,
    /// the most recent frame times, ms
    pub frame_ms: Vec<f32>,
}

pub fn sample(pid: u32) -> Option<Sample> {
    let now = now_100ns();
    let g = inner().lock().unwrap();
    let q = g.frames.get(&pid)?;
    let one_sec = 10_000_000;
    // events arrive in batches (the trace is flushed about once a second), so the newest ones are
    // always a little old: count back from the newest event, not from now
    let newest = *q.back()?;
    if now - newest > 3 * one_sec {
        return None; // the game stopped presenting
    }
    let in_last_sec = q.iter().filter(|t| newest - **t < one_sec).count();
    let recent: Vec<i64> = q.iter().copied().filter(|t| newest - *t <= 8 * one_sec).collect();
    let mut ms: Vec<f32> = recent.windows(2).map(|w| (w[1] - w[0]) as f32 / 10_000.0).collect();
    let low = if ms.len() >= 10 {
        let mut sorted = ms.clone();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let worst = sorted[(sorted.len() as f64 * 0.01) as usize];
        1000.0 / worst.max(0.1) as f64
    } else {
        in_last_sec as f64
    };
    let keep = ms.len().saturating_sub(60);
    ms.drain(..keep);
    Some(Sample { fps: in_last_sec as f64, low, frame_ms: ms })
}
