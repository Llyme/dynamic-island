//! GPU utilization for the hub's stat ring, from the Windows "GPU Engine"
//! performance counters (the same source Task Manager uses) -- works for
//! AMD/Intel/NVIDIA alike, unlike the Python version's nvidia-smi shell-out.
//!
//! Instance names look like `pid_123_luid_0x0_0x0000A1B2_phys_0_eng_0_engtype_3D`.
//! Utilization is summed per (adapter, engine type), then the busiest engine
//! type across adapters wins -- Task Manager's "GPU %" logic.

use crate::IslandState;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};
use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::*;

/// A PDH query keeps its wildcard expansion from when the counter was added,
/// so processes that start later would be missed -- rebuild it now and then.
const REBUILD_EVERY: u32 = 20;

pub struct GpuSampler {
    query: isize,
    counter: isize,
    samples: u32,
}

impl GpuSampler {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = 0isize;
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // rate counters need a baseline sample before the first real read
            PdhCollectQueryData(query);
            Some(Self { query, counter, samples: 0 })
        }
    }

    fn read(&mut self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            self.samples += 1;
            let mut size = 0u32;
            let mut count = 0u32;
            // first call reports the buffer size it needs
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if size == 0 {
                return None;
            }
            // u64-backed so the item structs are aligned
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return None;
            }
            let mut per_engine: HashMap<String, f64> = HashMap::new();
            for i in 0..count as usize {
                let item = &*items.add(i);
                if item.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = item.szName.to_string().unwrap_or_default();
                let (Some(luid), Some(engtype)) = (
                    name.split("_luid_").nth(1).and_then(|s| s.split("_phys_").next()),
                    name.rsplit("_engtype_").next(),
                ) else {
                    continue;
                };
                *per_engine.entry(format!("{luid}/{engtype}")).or_insert(0.0) +=
                    item.FmtValue.Anonymous.doubleValue;
            }
            Some(per_engine.values().cloned().fold(0.0, f64::max).min(100.0))
        }
    }
}

impl Drop for GpuSampler {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

#[derive(Default)]
pub struct GpuState {
    sampler: std::sync::Mutex<Option<GpuSampler>>,
}

/// GPU busy %, or None when the counters aren't available.
#[tauri::command]
pub fn get_gpu_pct(window: WebviewWindow) -> Option<f64> {
    let state = window.state::<Arc<IslandState>>();
    let mut guard = state.gpu.sampler.lock().unwrap();
    if guard.as_ref().map_or(true, |s| s.samples >= REBUILD_EVERY) {
        *guard = GpuSampler::open();
        // brand-new query has no delta yet -- report nothing until next poll
        return None;
    }
    guard.as_mut().and_then(|s| s.read())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_gpu_counter() {
        let mut s = GpuSampler::open().expect("open");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let v = s.read();
        println!("gpu pct = {v:?}");
        assert!(v.is_some());
    }
}
