//! Per-game numbers for the Game card: CPU and memory from the game's process, GPU load and video
//! memory from the Windows GPU counters (the same source Task Manager uses), filtered to its pid.
//! Read only while the hub shows the game card, about once a second.

use crate::IslandState;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{Manager, WebviewWindow};
use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::*;

/// A PDH query keeps its wildcard expansion from when the counter was added, so processes that
/// start later would be missed: rebuild it now and then.
const REBUILD_EVERY: u32 = 20;

pub struct PidGpu {
    query: isize,
    engine: isize,
    memory: Option<isize>,
    samples: u32,
}

/// (instance name, value) of every instance of a wildcard counter
unsafe fn read_array(counter: isize) -> Vec<(String, f64)> {
    let mut size = 0u32;
    let mut count = 0u32;
    // the first call reports the buffer size it needs
    PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
    if size == 0 {
        return Vec::new();
    }
    // u64-backed so the item structs are aligned
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let item = &*items.add(i);
        if item.FmtValue.CStatus != 0 {
            continue;
        }
        out.push((item.szName.to_string().unwrap_or_default(), item.FmtValue.Anonymous.doubleValue));
    }
    out
}

/// `pid_1234_luid_...` -> 1234
fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

impl PidGpu {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut engine = 0isize;
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut engine) != 0 {
                let _ = PdhCloseQuery(query);
                return None;
            }
            let mut mem = 0isize;
            let memory = (PdhAddEnglishCounterW(query, w!("\\GPU Process Memory(*)\\Dedicated Usage"), 0, &mut mem) == 0).then_some(mem);
            // rate counters need a baseline sample before the first real read
            PdhCollectQueryData(query);
            Some(Self { query, engine, memory, samples: 0 })
        }
    }

    /// (GPU busy % per pid, dedicated video memory in bytes per pid)
    fn read(&mut self) -> Option<(HashMap<u32, f64>, HashMap<u32, f64>)> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            self.samples += 1;
            // summed per (pid, engine type); a pid's load is its busiest engine type, as in Task Manager
            let mut per_engine: HashMap<(u32, String), f64> = HashMap::new();
            for (name, v) in read_array(self.engine) {
                let (Some(pid), Some(engtype)) = (pid_of(&name), name.rsplit("_engtype_").next()) else { continue };
                *per_engine.entry((pid, engtype.to_string())).or_insert(0.0) += v;
            }
            let mut gpu: HashMap<u32, f64> = HashMap::new();
            for ((pid, _), v) in per_engine {
                let e = gpu.entry(pid).or_insert(0.0);
                *e = e.max(v.min(100.0));
            }
            let mut vram: HashMap<u32, f64> = HashMap::new();
            if let Some(mem) = self.memory {
                for (name, v) in read_array(mem) {
                    if let Some(pid) = pid_of(&name) {
                        *vram.entry(pid).or_insert(0.0) += v;
                    }
                }
            }
            Some((gpu, vram))
        }
    }
}

impl Drop for PidGpu {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

pub struct GameMeter {
    sys: System,
    gpu: Option<PidGpu>,
}

impl Default for GameMeter {
    fn default() -> Self {
        Self { sys: System::new(), gpu: None }
    }
}

#[derive(Serialize)]
pub struct GameStat {
    pid: u32,
    /// share of the whole CPU the game uses
    cpu_pct: f32,
    ram_gb: f64,
    /// share of the installed memory
    ram_pct: f32,
    gpu_pct: Option<f64>,
    vram_gb: Option<f64>,
    /// frames per second, the slowest 1% of frames as FPS, and the latest frame times (ms); none
    /// when the game is not presenting through DXGI or the trace is not running
    fps: Option<f64>,
    low_fps: Option<f64>,
    frame_ms: Vec<f32>,
    /// Windows refused the trace (no rights): there will be no FPS
    fps_denied: bool,
}

#[tauri::command]
pub fn get_game_stats(window: WebviewWindow, pids: Vec<u32>) -> Vec<GameStat> {
    let state = window.state::<Arc<IslandState>>();
    let mut m = state.game_meter.lock().unwrap();
    crate::fps::touch(&pids);
    if pids.is_empty() {
        m.gpu = None; // nothing to measure: let the counters go
        return Vec::new();
    }
    let denied = crate::fps::denied();
    let list: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    m.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&list), true, ProcessRefreshKind::new().with_cpu().with_memory());
    m.sys.refresh_memory();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
    let total = m.sys.total_memory() as f64;
    let gb = 1024.0 * 1024.0 * 1024.0;

    if m.gpu.as_ref().map_or(true, |g| g.samples >= REBUILD_EVERY) {
        m.gpu = PidGpu::open(); // a new query has no delta yet: no GPU numbers until the next read
    }
    let gpu = m.gpu.as_mut().and_then(|g| g.read());

    pids.iter()
        .map(|&pid| {
            let p = m.sys.process(Pid::from_u32(pid));
            let ram = p.map_or(0.0, |p| p.memory() as f64);
            let f = crate::fps::sample(pid);
            GameStat {
                pid,
                cpu_pct: p.map_or(0.0, |p| p.cpu_usage() / cores).min(100.0),
                ram_gb: ram / gb,
                ram_pct: if total > 0.0 { (ram / total * 100.0) as f32 } else { 0.0 },
                gpu_pct: gpu.as_ref().map(|(g, _)| g.get(&pid).copied().unwrap_or(0.0)),
                vram_gb: gpu.as_ref().and_then(|(_, v)| v.get(&pid)).map(|b| b / gb),
                fps: f.as_ref().map(|f| f.fps),
                low_fps: f.as_ref().map(|f| f.low),
                frame_ms: f.map(|f| f.frame_ms).unwrap_or_default(),
                fps_denied: denied,
            }
        })
        .collect()
}
