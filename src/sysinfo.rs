//! System information for `GET /v1/local/system`: OS, CPU, memory, GPU
//! (compute-device registry), process, and the effective server bind. Data
//! collection (`probe`) is platform-specific (Windows APIs via `windows-sys`,
//! with a portable fallback); JSON assembly (`to_json`) is pure and
//! unit-tested against a fake `SystemSnapshot` so it never needs a real
//! machine probe or a loaded model.
//!
//! GPU devices come from `transcribe_cpp::devices()`, the native process-
//! global compute-device registry. In this crate's static build the
//! registered backends (including Vulkan, when the driver is present) are
//! already enumerable at process start -- no model load is required to see
//! them. `vulkan_available` is `backend_available(Backend::Vulkan)`
//! independently, so a caller can tell "Vulkan usable" from "Vulkan device
//! enumerated" even if a future dynamic-backend build can't enumerate before
//! a model load.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use transcribe_cpp::{backend_available, Backend};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OsInfo {
    pub name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CpuInfo {
    pub model: Option<String>,
    pub logical_cores: Option<u32>,
    pub physical_cores: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryInfo {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuDeviceInfo {
    pub name: String,
    pub backend: String,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuInfo {
    pub vulkan_available: bool,
    pub devices: Vec<GpuDeviceInfo>,
    /// Set when device enumeration could not run without a loaded model (not
    /// the case for this crate's static build, but kept so a future backend
    /// mode can say so honestly instead of fabricating devices).
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub uptime_ms: u64,
    pub rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemSnapshot {
    pub os: OsInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub gpu: GpuInfo,
    pub process: ProcessInfo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSection {
    pub version: String,
    pub api_level: u32,
    pub host: String,
    pub port: u16,
    pub data_dir: String,
}

/// Assemble the `GET /v1/local/system` JSON body from a snapshot and the
/// server's own bind info. Fields the snapshot couldn't obtain are omitted
/// entirely (never reported as `null` or a fabricated default), per the
/// "omit unsupported/unknown" convention used across this API.
pub fn to_json(snapshot: &SystemSnapshot, server: &ServerSection) -> Value {
    let mut os = json!({});
    if let Some(name) = &snapshot.os.name {
        os["name"] = json!(name);
    }
    if let Some(version) = &snapshot.os.version {
        os["version"] = json!(version);
    }

    let mut cpu = json!({});
    if let Some(model) = &snapshot.cpu.model {
        cpu["model"] = json!(model);
    }
    if let Some(logical) = snapshot.cpu.logical_cores {
        cpu["logical_cores"] = json!(logical);
    }
    if let Some(physical) = snapshot.cpu.physical_cores {
        cpu["physical_cores"] = json!(physical);
    }

    let mut memory = json!({});
    if let Some(total) = snapshot.memory.total_bytes {
        memory["total_bytes"] = json!(total);
    }
    if let Some(available) = snapshot.memory.available_bytes {
        memory["available_bytes"] = json!(available);
    }

    let devices: Vec<Value> = snapshot
        .gpu
        .devices
        .iter()
        .map(|device| {
            let mut v = json!({
                "name": device.name,
                "backend": device.backend,
            });
            if let Some(mem) = device.memory_bytes {
                v["memory_bytes"] = json!(mem);
            }
            v
        })
        .collect();
    let mut gpu = json!({
        "vulkan_available": snapshot.gpu.vulkan_available,
        "devices": devices,
    });
    if let Some(note) = &snapshot.gpu.note {
        gpu["note"] = json!(note);
    }

    let mut process = json!({
        "pid": snapshot.process.pid,
        "uptime_ms": snapshot.process.uptime_ms,
    });
    if let Some(rss) = snapshot.process.rss_bytes {
        process["rss_bytes"] = json!(rss);
    }

    json!({
        "os": os,
        "cpu": cpu,
        "memory": memory,
        "gpu": gpu,
        "process": process,
        "server": {
            "version": server.version,
            "api_level": server.api_level,
            "host": server.host,
            "port": server.port,
            "data_dir": server.data_dir,
        },
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fallback process start time, captured the first time anything in this
/// process asks for it, for platforms with no OS-reported process creation
/// time (see `platform::process_start_ms`). On Windows this is unused --
/// `GetProcessTimes` gives the real value instead.
static PROCESS_START_MS_FALLBACK: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

fn process_start_ms() -> i64 {
    platform::process_start_ms().unwrap_or_else(|| *PROCESS_START_MS_FALLBACK.get_or_init(now_ms))
}

/// GPU info shared by every platform: query the native compute-device
/// registry directly, independent of whether a model is loaded.
fn gpu_info() -> GpuInfo {
    let vulkan_available = backend_available(Backend::Vulkan);
    let devices = transcribe_cpp::devices()
        .into_iter()
        .filter(|device| device.kind != "cpu")
        .map(|device| {
            let name = if device.description.trim().is_empty() {
                device.name.clone()
            } else {
                device.description.clone()
            };
            GpuDeviceInfo {
                name,
                backend: device.kind.clone(),
                memory_bytes: if device.memory_total > 0 {
                    Some(device.memory_total)
                } else {
                    None
                },
            }
        })
        .collect();
    GpuInfo {
        vulkan_available,
        devices,
        note: None,
    }
}

/// Collect a full snapshot for the current process/machine.
pub fn probe() -> SystemSnapshot {
    let (os, cpu, memory, rss_bytes) = platform::probe_hardware();
    SystemSnapshot {
        os,
        cpu,
        memory,
        gpu: gpu_info(),
        process: ProcessInfo {
            pid: std::process::id(),
            uptime_ms: (now_ms() - process_start_ms()).max(0) as u64,
            rss_bytes,
        },
    }
}

#[cfg(windows)]
mod platform {
    use super::{CpuInfo, MemoryInfo, OsInfo};
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformation, GetSystemInfo, GlobalMemoryStatusEx, RelationProcessorCore,
        MEMORYSTATUSEX, SYSTEM_INFO, SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn probe_hardware() -> (OsInfo, CpuInfo, MemoryInfo, Option<u64>) {
        (os_info(), cpu_info(), memory_info(), rss_bytes())
    }

    fn reg_string(sub_key: &str, value_name: &str) -> Option<String> {
        // `\0`-terminated UTF-16, as the Win32 registry API requires.
        let sub_key_w: Vec<u16> = sub_key.encode_utf16().chain(std::iter::once(0)).collect();
        let value_name_w: Vec<u16> = value_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            let open_status = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_key_w.as_ptr(),
                0,
                KEY_READ,
                &mut hkey,
            );
            if open_status != 0 {
                return None;
            }
            let mut buf = [0u16; 512];
            let mut size_bytes = (buf.len() * 2) as u32;
            let status = RegQueryValueExW(
                hkey,
                value_name_w.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut size_bytes,
            );
            RegCloseKey(hkey);
            if status != 0 {
                return None;
            }
            let chars = (size_bytes as usize / 2).min(buf.len());
            let raw = String::from_utf16_lossy(&buf[..chars]);
            let trimmed = raw.trim_end_matches('\0').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    }

    /// `UBR` (Update Build Revision) is stored as a `REG_DWORD`, not a
    /// string -- reading it through [`reg_string`] silently misdecodes the
    /// 4 raw bytes as UTF-16 garbage. Read it as a little-endian `u32`
    /// instead.
    fn reg_dword(sub_key: &str, value_name: &str) -> Option<u32> {
        let sub_key_w: Vec<u16> = sub_key.encode_utf16().chain(std::iter::once(0)).collect();
        let value_name_w: Vec<u16> = value_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            let open_status = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_key_w.as_ptr(),
                0,
                KEY_READ,
                &mut hkey,
            );
            if open_status != 0 {
                return None;
            }
            let mut value: u32 = 0;
            let mut size_bytes = size_of::<u32>() as u32;
            let status = RegQueryValueExW(
                hkey,
                value_name_w.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut value as *mut u32 as *mut u8,
                &mut size_bytes,
            );
            RegCloseKey(hkey);
            if status != 0 || size_bytes != size_of::<u32>() as u32 {
                return None;
            }
            Some(value)
        }
    }

    fn os_info() -> OsInfo {
        const KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        let product_name = reg_string(KEY, "ProductName");
        let build = reg_string(KEY, "CurrentBuildNumber");
        let ubr = reg_dword(KEY, "UBR");
        // Windows 11 still reports ProductName "Windows 10 ..." on older
        // registries; the build number is the reliable 10-vs-11 signal
        // (>= 22000).
        let name = match (&product_name, &build) {
            (Some(_), Some(build_str)) => match build_str.parse::<u32>() {
                Ok(build_num) if build_num >= 22000 => Some("Windows 11".to_string()),
                Ok(_) => Some("Windows 10".to_string()),
                Err(_) => product_name.clone(),
            },
            _ => product_name.clone(),
        };
        let version = build.map(|build| match ubr {
            Some(ubr) => format!("{build}.{ubr}"),
            None => build,
        });
        OsInfo { name, version }
    }

    fn cpu_info() -> CpuInfo {
        let model = reg_string(
            "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
            "ProcessorNameString",
        );
        let logical_cores = unsafe {
            let mut info: SYSTEM_INFO = std::mem::zeroed();
            GetSystemInfo(&mut info);
            Some(info.dwNumberOfProcessors)
        };
        let physical_cores = physical_core_count();
        CpuInfo {
            model,
            logical_cores,
            physical_cores,
        }
    }

    fn physical_core_count() -> Option<u32> {
        unsafe {
            let mut len: u32 = 0;
            // First call with a null buffer to learn the required size.
            GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len);
            if len == 0 {
                return None;
            }
            let entry_size = size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
            let count = (len as usize).div_ceil(entry_size);
            let mut buf: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> = Vec::with_capacity(count);
            let ok = GetLogicalProcessorInformation(buf.as_mut_ptr(), &mut len);
            if ok == 0 {
                return None;
            }
            let actual_count = (len as usize) / entry_size;
            buf.set_len(actual_count);
            let physical = buf
                .iter()
                .filter(|entry| entry.Relationship == RelationProcessorCore)
                .count();
            if physical == 0 {
                None
            } else {
                Some(physical as u32)
            }
        }
    }

    fn memory_info() -> MemoryInfo {
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                MemoryInfo {
                    total_bytes: Some(status.ullTotalPhys),
                    available_bytes: Some(status.ullAvailPhys),
                }
            } else {
                MemoryInfo::default()
            }
        }
    }

    fn rss_bytes() -> Option<u64> {
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let handle: HANDLE = GetCurrentProcess();
            if GetProcessMemoryInfo(handle, &mut counters, counters.cb) != 0 {
                Some(counters.WorkingSetSize as u64)
            } else {
                None
            }
        }
    }

    /// The process's actual creation time (Unix epoch ms), from
    /// `GetProcessTimes` -- not an approximation seeded by the first
    /// snapshot request, so `uptime_ms` is correct even on the very first
    /// `GET /v1/local/system` call shortly after startup.
    pub(super) fn process_start_ms() -> Option<i64> {
        // FILETIME: 100-ns intervals since 1601-01-01, split into two u32s.
        const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
        unsafe {
            let mut creation: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
            let mut exit: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
            let mut kernel: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
            let mut user: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
            let handle: HANDLE = GetCurrentProcess();
            let ok = windows_sys::Win32::System::Threading::GetProcessTimes(
                handle,
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            );
            if ok == 0 {
                return None;
            }
            let ticks_100ns =
                ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
            let unix_100ns = ticks_100ns as i64 - EPOCH_DIFF_100NS;
            Some(unix_100ns / 10_000)
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{CpuInfo, MemoryInfo, OsInfo};

    /// Portable fallback for non-Windows dev/test builds (this crate ships
    /// for Windows only -- see `AGENTS.md`). Reports what the standard
    /// library can give us and omits the rest rather than fabricating it.
    pub(super) fn probe_hardware() -> (OsInfo, CpuInfo, MemoryInfo, Option<u64>) {
        let logical_cores = std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as u32);
        let os = OsInfo {
            name: Some(std::env::consts::OS.to_string()),
            version: None,
        };
        let cpu = CpuInfo {
            model: None,
            logical_cores,
            physical_cores: None,
        };
        (os, cpu, MemoryInfo::default(), None)
    }

    /// No portable process-creation-time API in `std`; `probe()` falls back
    /// to approximating it from the first snapshot request instead.
    pub(super) fn process_start_ms() -> Option<i64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ServerSection {
        ServerSection {
            version: "0.1.0".to_string(),
            api_level: 1,
            host: "127.0.0.1".to_string(),
            port: 54321,
            data_dir: "C:\\data".to_string(),
        }
    }

    #[test]
    fn full_snapshot_reports_every_field() {
        let snapshot = SystemSnapshot {
            os: OsInfo {
                name: Some("Windows 11".to_string()),
                version: Some("10.0.26200".to_string()),
            },
            cpu: CpuInfo {
                model: Some("Fake CPU".to_string()),
                logical_cores: Some(16),
                physical_cores: Some(8),
            },
            memory: MemoryInfo {
                total_bytes: Some(34_000_000_000),
                available_bytes: Some(12_000_000_000),
            },
            gpu: GpuInfo {
                vulkan_available: true,
                devices: vec![GpuDeviceInfo {
                    name: "Fake GPU".to_string(),
                    backend: "vulkan".to_string(),
                    memory_bytes: Some(8_000_000_000),
                }],
                note: None,
            },
            process: ProcessInfo {
                pid: 1234,
                uptime_ms: 5000,
                rss_bytes: Some(200_000_000),
            },
        };
        let value = to_json(&snapshot, &server());
        assert_eq!(value["os"]["name"], json!("Windows 11"));
        assert_eq!(value["os"]["version"], json!("10.0.26200"));
        assert_eq!(value["cpu"]["model"], json!("Fake CPU"));
        assert_eq!(value["cpu"]["logical_cores"], json!(16));
        assert_eq!(value["cpu"]["physical_cores"], json!(8));
        assert_eq!(value["memory"]["total_bytes"], json!(34_000_000_000u64));
        assert_eq!(value["memory"]["available_bytes"], json!(12_000_000_000u64));
        assert_eq!(value["gpu"]["vulkan_available"], json!(true));
        assert_eq!(value["gpu"]["devices"][0]["name"], json!("Fake GPU"));
        assert_eq!(value["gpu"]["devices"][0]["backend"], json!("vulkan"));
        assert_eq!(
            value["gpu"]["devices"][0]["memory_bytes"],
            json!(8_000_000_000u64)
        );
        assert_eq!(value["process"]["pid"], json!(1234));
        assert_eq!(value["process"]["uptime_ms"], json!(5000));
        assert_eq!(value["process"]["rss_bytes"], json!(200_000_000));
        assert_eq!(value["server"]["version"], json!("0.1.0"));
        assert_eq!(value["server"]["host"], json!("127.0.0.1"));
        assert_eq!(value["server"]["port"], json!(54321));
        assert_eq!(value["server"]["data_dir"], json!("C:\\data"));
    }

    #[test]
    fn missing_fields_are_omitted_not_fabricated() {
        let snapshot = SystemSnapshot {
            os: OsInfo::default(),
            cpu: CpuInfo {
                model: None,
                logical_cores: Some(4),
                physical_cores: None,
            },
            memory: MemoryInfo::default(),
            gpu: GpuInfo {
                vulkan_available: false,
                devices: vec![],
                note: Some("no GPU probe available".to_string()),
            },
            process: ProcessInfo {
                pid: 1,
                uptime_ms: 0,
                rss_bytes: None,
            },
        };
        let value = to_json(&snapshot, &server());
        assert!(value["os"].get("name").is_none());
        assert!(value["os"].get("version").is_none());
        assert!(value["cpu"].get("model").is_none());
        assert_eq!(value["cpu"]["logical_cores"], json!(4));
        assert!(value["cpu"].get("physical_cores").is_none());
        assert!(value["memory"].get("total_bytes").is_none());
        assert!(value["memory"].get("available_bytes").is_none());
        assert!(value["process"].get("rss_bytes").is_none());
        assert_eq!(value["gpu"]["devices"], json!([]));
        assert_eq!(value["gpu"]["note"], json!("no GPU probe available"));
    }

    #[test]
    fn real_probe_does_not_panic_and_reports_something() {
        // Smoke test on whatever machine runs the suite: no assertions on
        // exact values (those vary per machine/CI), just that assembling the
        // JSON from a live probe doesn't panic and includes the required
        // top-level shape.
        let snapshot = probe();
        let value = to_json(&snapshot, &server());
        for key in ["os", "cpu", "memory", "gpu", "process", "server"] {
            assert!(value.get(key).is_some(), "missing top-level key {key}");
        }
        assert!(value["gpu"]["devices"].is_array());
    }
}
