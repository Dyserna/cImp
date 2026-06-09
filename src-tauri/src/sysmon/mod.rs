//! System-monitor sampler for the bottom-bar stats panel.
//!
//! Provides a one-shot `sample()` returning current CPU%, memory%, NVIDIA GPU
//! stats, and network throughput. The frontend polls this (default every 1s)
//! and keeps its own history for the sparklines — the backend is stateless
//! beyond what the delta-based metrics (CPU usage, network bytes) require.
//!
//! Data sources:
//!   - CPU% / memory% / network bytes: `sysinfo` (pure-Rust, no admin).
//!   - GPU utilization / memory / temperature: `nvml-wrapper` (NVIDIA only;
//!     loads `nvml.dll` at runtime). Absent / non-NVIDIA → `gpu: None`.
//!
//! CPU temperature is intentionally not collected: reading it on Windows needs
//! an elevated kernel-level helper (LibreHardwareMonitor), which we declined to
//! avoid running with admin / tripping antivirus. GPU temperature is available
//! without admin via NVML.

use std::sync::Mutex;
use std::time::Instant;

use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::Nvml;
use serde::Serialize;
use sysinfo::{Networks, System};
use tracing::debug;

#[derive(Serialize, Clone, Debug)]
pub struct GpuStats {
    /// GPU core utilization, 0–100.
    pub util_pct: f32,
    /// VRAM used / total, 0–100.
    pub mem_pct: f32,
    /// GPU temperature in Celsius.
    pub temp_c: f32,
}

#[derive(Serialize, Clone, Debug)]
pub struct NetStats {
    /// Bytes/sec received since the last sample.
    pub down_bps: u64,
    /// Bytes/sec transmitted since the last sample.
    pub up_bps: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct SystemStatsSnapshot {
    /// Global CPU usage, 0–100.
    pub cpu_pct: f32,
    /// Used / total physical memory, 0–100.
    pub mem_pct: f32,
    /// NVIDIA GPU stats, or `None` when no NVIDIA GPU / NVML.
    pub gpu: Option<GpuStats>,
    pub net: NetStats,
}

struct Inner {
    sys: System,
    networks: Networks,
    /// When the network counters were last read — divides the byte delta to
    /// get a per-second rate regardless of jitter in the poll cadence.
    last_net: Instant,
}

pub struct SystemStatsState {
    inner: Mutex<Inner>,
    /// `None` when NVML couldn't initialize (no NVIDIA driver / GPU). Init once
    /// and reuse, per nvml-wrapper guidance.
    nvml: Option<Nvml>,
}

impl SystemStatsState {
    pub fn new() -> Self {
        let mut sys = System::new();
        // Prime the CPU baseline so the first real sample (≥1 poll later) is
        // accurate rather than 0% (sysinfo computes usage as a delta).
        sys.refresh_cpu_usage();
        let networks = Networks::new_with_refreshed_list();
        let nvml = match Nvml::init() {
            Ok(n) => Some(n),
            Err(e) => {
                debug!(error = %e, "nvml init failed; GPU stats disabled");
                None
            }
        };
        Self {
            inner: Mutex::new(Inner {
                sys,
                networks,
                last_net: Instant::now(),
            }),
            nvml,
        }
    }

    pub fn sample(&self) -> SystemStatsSnapshot {
        let (cpu_pct, mem_pct, net) = {
            let mut guard = self.inner.lock().expect("sysmon mutex poisoned");

            guard.sys.refresh_cpu_usage();
            let cpu_pct = guard.sys.global_cpu_usage();

            guard.sys.refresh_memory();
            let total = guard.sys.total_memory();
            let used = guard.sys.used_memory();
            let mem_pct = if total > 0 {
                (used as f32 / total as f32) * 100.0
            } else {
                0.0
            };

            // `received()`/`transmitted()` are bytes since the previous
            // refresh; divide by elapsed wall time for a stable bytes/sec.
            guard.networks.refresh(false);
            let elapsed = guard.last_net.elapsed().as_secs_f64().max(0.001);
            guard.last_net = Instant::now();
            let (mut down, mut up) = (0u64, 0u64);
            for (_name, data) in &guard.networks {
                down += data.received();
                up += data.transmitted();
            }
            let net = NetStats {
                down_bps: (down as f64 / elapsed) as u64,
                up_bps: (up as f64 / elapsed) as u64,
            };

            (cpu_pct, mem_pct, net)
        };

        SystemStatsSnapshot {
            cpu_pct,
            mem_pct,
            gpu: self.sample_gpu(),
            net,
        }
    }

    /// Read GPU 0 via NVML. Any error (no device, transient query failure)
    /// collapses to `None` so the UI hides the GPU section rather than erroring.
    fn sample_gpu(&self) -> Option<GpuStats> {
        let nvml = self.nvml.as_ref()?;
        let dev = nvml.device_by_index(0).ok()?;
        let util = dev.utilization_rates().ok()?;
        let mem = dev.memory_info().ok()?;
        let temp = dev.temperature(TemperatureSensor::Gpu).ok()?;
        let mem_pct = if mem.total > 0 {
            (mem.used as f32 / mem.total as f32) * 100.0
        } else {
            0.0
        };
        Some(GpuStats {
            util_pct: util.gpu as f32,
            mem_pct,
            temp_c: temp as f32,
        })
    }
}

impl Default for SystemStatsState {
    fn default() -> Self {
        Self::new()
    }
}
