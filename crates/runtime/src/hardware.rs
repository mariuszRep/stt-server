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
    /// `nvidia-smi`'s reported VRAM size, in bytes. `None` whenever
    /// `has_nvidia_gpu` is `false`, or (rarely) if the query succeeded but
    /// this specific field couldn't be parsed — never blocks the rest of
    /// the report on a partial/malformed reading.
    pub vram_bytes: Option<u64>,
    pub cpu_cores: usize,
    pub cpu_architecture: String,
    pub total_ram_bytes: u64,
}

/// Detect current-machine hardware capability. Never fails: absence of a
/// signal (no `nvidia-smi`, RAM unreadable) degrades to a conservative
/// CPU-only report rather than an error, since hardware detection must not
/// block the control plane from starting.
pub fn detect() -> HardwareReport {
    let (has_nvidia_gpu, gpu_name, driver_version, vram_bytes) = detect_nvidia_gpu();
    HardwareReport {
        has_nvidia_gpu,
        gpu_name,
        driver_version,
        vram_bytes,
        cpu_cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        cpu_architecture: std::env::consts::ARCH.to_string(),
        total_ram_bytes: total_ram_bytes(),
    }
}

fn detect_nvidia_gpu() -> (bool, Option<String>, Option<String>, Option<u64>) {
    detect_nvidia_gpu_memory().map_or((false, None, None, None), |(name, driver, total, _free)| {
        (true, name, driver, total)
    })
}

/// `(name, driver_version, total_bytes, free_bytes)`, each individually
/// absent whenever `nvidia-smi`'s CSV output is short or unparseable.
type NvidiaGpuMemory = (Option<String>, Option<String>, Option<u64>, Option<u64>);

/// Shared by `detect_nvidia_gpu` (total VRAM, part of the one-time hardware
/// report) and `snapshot_memory` (total + *free* VRAM, queried live before
/// a pin/eviction decision) so both read from the same `nvidia-smi` call
/// shape instead of drifting apart. `None` whenever there's no NVIDIA GPU or
/// the query fails — never blocks the caller on a missing signal.
fn detect_nvidia_gpu_memory() -> Option<NvidiaGpuMemory> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total,memory.free",
            "--format=csv,noheader",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    let mut parts = first_line.splitn(4, ',');
    let name = parts.next().map(|s| s.trim().to_string());
    let driver = parts.next().map(|s| s.trim().to_string());
    let total_bytes = parts.next().and_then(|s| parse_mib_to_bytes(s.trim()));
    let free_bytes = parts.next().and_then(|s| parse_mib_to_bytes(s.trim()));
    Some((name, driver, total_bytes, free_bytes))
}

/// Parses `nvidia-smi`'s `memory.total` CSV field, e.g. `"4096 MiB"`, into
/// bytes. `None` on any unexpected shape — a VRAM-size parse failure should
/// never take down the rest of hardware detection.
fn parse_mib_to_bytes(field: &str) -> Option<u64> {
    let mib_str = field.strip_suffix("MiB")?.trim();
    let mib: u64 = mib_str.parse().ok()?;
    Some(mib * 1024 * 1024)
}

fn total_ram_bytes() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySnapshot {
    pub total_ram_bytes: u64,
    /// System-reported free-for-use RAM right now (`sysinfo`'s
    /// `available_memory`, which already accounts for reclaimable
    /// cache/buffers the way the OS itself would offer it to a new
    /// allocation — not just literally-unused pages).
    pub available_ram_bytes: u64,
    pub total_vram_bytes: Option<u64>,
    /// `None` whenever there's no NVIDIA GPU, or a CPU-only build is what's
    /// actually running -- a caller deciding whether to keep a GPU model
    /// pinned should treat "no signal" as "can't reason about VRAM here",
    /// not as "zero used".
    pub available_vram_bytes: Option<u64>,
}

/// Cheap, repeatable read of *currently available* capacity, distinct from
/// `detect()`'s one-time hardware report -- meant to be called before each
/// pin/eviction decision (see concurrent-multi-provider-serving's
/// capacity-aware pinning), not just once at startup. Never fails: absent
/// signals surface as `None`/best-effort defaults, same policy as `detect`.
pub fn snapshot_memory() -> MemorySnapshot {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let (total_vram_bytes, available_vram_bytes) = detect_nvidia_gpu_memory()
        .map(|(_, _, total, free)| (total, free))
        .unwrap_or((None, None));
    MemorySnapshot {
        total_ram_bytes: sys.total_memory(),
        available_ram_bytes: sys.available_memory(),
        total_vram_bytes,
        available_vram_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_never_panics_and_reports_at_least_one_cpu_core() {
        let report = detect();
        assert!(report.cpu_cores >= 1);
    }

    #[test]
    fn snapshot_memory_never_panics_and_reports_plausible_ram() {
        let snapshot = snapshot_memory();
        assert!(snapshot.total_ram_bytes > 0);
        // available <= total is the only invariant that holds on every
        // machine this could run on -- exact values are host-dependent.
        assert!(snapshot.available_ram_bytes <= snapshot.total_ram_bytes);
    }

    #[test]
    fn detect_reports_a_non_empty_cpu_architecture() {
        assert!(!detect().cpu_architecture.is_empty());
    }

    #[test]
    fn parse_mib_to_bytes_handles_real_nvidia_smi_output() {
        // Real output captured from this machine's nvidia-smi during implementation.
        assert_eq!(parse_mib_to_bytes("4096 MiB"), Some(4096 * 1024 * 1024));
    }

    #[test]
    fn parse_mib_to_bytes_rejects_unexpected_shapes() {
        assert_eq!(parse_mib_to_bytes(""), None);
        assert_eq!(parse_mib_to_bytes("N/A"), None);
        assert_eq!(parse_mib_to_bytes("4096 GiB"), None);
        assert_eq!(parse_mib_to_bytes("not-a-number MiB"), None);
    }
}
