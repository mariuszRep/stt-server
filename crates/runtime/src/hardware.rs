//! Coarse local hardware capability detection.
//!
//! This deliberately stops at "does this machine plausibly support a
//! GPU-accelerated runtime variant" — it does not attempt to reproduce
//! `ctranslate2`'s CUDA/cuDNN runtime probing (loading the actual CUDA
//! libraries, checking supported compute types). That validation only
//! matters inside a running managed runtime, which already does it and
//! self-reports via its own `GET /v1/config` (`cuda_available`,
//! `cuda_runtime_ok`, `cuda_error`) with a CPU fallback on failure. This
//! module only needs enough signal to decide which install artifact to
//! fetch and what to recommend.

use std::process::Command;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareReport {
    pub has_nvidia_gpu: bool,
    pub gpu_name: Option<String>,
    pub driver_version: Option<String>,
    pub cpu_cores: usize,
    pub total_ram_bytes: u64,
}

/// Detect current-machine hardware capability. Never fails: absence of a
/// signal (no `nvidia-smi`, RAM unreadable) degrades to a conservative
/// CPU-only report rather than an error, since hardware detection must not
/// block the control plane from starting.
pub fn detect() -> HardwareReport {
    let (has_nvidia_gpu, gpu_name, driver_version) = detect_nvidia_gpu();
    HardwareReport {
        has_nvidia_gpu,
        gpu_name,
        driver_version,
        cpu_cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        total_ram_bytes: total_ram_bytes(),
    }
}

fn detect_nvidia_gpu() -> (bool, Option<String>, Option<String>) {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name,driver_version", "--format=csv,noheader"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let first_line = text.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                return (false, None, None);
            }
            let mut parts = first_line.splitn(2, ',');
            let name = parts.next().map(|s| s.trim().to_string());
            let driver = parts.next().map(|s| s.trim().to_string());
            (true, name, driver)
        }
        _ => (false, None, None),
    }
}

fn total_ram_bytes() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_never_panics_and_reports_at_least_one_cpu_core() {
        let report = detect();
        assert!(report.cpu_cores >= 1);
    }
}
