//! CPU / RAM utilization for the hub's stat rings. The `System` handle is kept
//! across calls on purpose: CPU usage is a delta between two refreshes, so a
//! fresh `System` per call would always report 0%.

use crate::IslandState;
use serde::Serialize;
use std::sync::Arc;
use sysinfo::System;
use tauri::{Manager, WebviewWindow};

#[derive(Serialize)]
pub struct SysStats {
    pub cpu_pct: f32,
    pub ram_pct: f32,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
}

pub fn new_system() -> System {
    let mut sys = System::new();
    sys.refresh_cpu_usage(); // prime the sampler; the first real read comes later
    sys
}

#[tauri::command]
pub fn get_sys_stats(window: WebviewWindow) -> SysStats {
    let state = window.state::<Arc<IslandState>>();
    let mut sys = state.sys.lock().unwrap();
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    let gb = 1024.0 * 1024.0 * 1024.0;
    SysStats {
        cpu_pct: sys.global_cpu_usage(),
        ram_pct: if total > 0.0 { (used / total * 100.0) as f32 } else { 0.0 },
        ram_used_gb: used / gb,
        ram_total_gb: total / gb,
    }
}
