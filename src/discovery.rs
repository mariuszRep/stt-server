//! Single-instance discovery: `<data dir>/server.json` records the running
//! instance (pid, host, port, version, started_at) and `<data dir>/server.lock`
//! is held exclusively for the process lifetime so a second instance on the
//! same data dir fails fast with a clear error (exit code 3).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerInfo {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub version: String,
    pub started_at: i64,
}

pub fn server_json_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.json")
}

pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.lock")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ServerInfo {
    pub fn new(host: String, port: u16) -> Self {
        ServerInfo {
            pid: std::process::id(),
            host,
            port,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at: now_ms(),
        }
    }
}

/// Atomically write `server.json`: write to a temp file in the same
/// directory, then rename over the target (rename is atomic on the same
/// filesystem on both Windows and Unix).
pub fn write_server_json(data_dir: &Path, info: &ServerInfo) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let target = server_json_path(data_dir);
    let tmp = data_dir.join(format!("server.json.{}.tmp", std::process::id()));
    {
        let mut file = File::create(&tmp)?;
        let json = serde_json::to_string_pretty(info).map_err(std::io::Error::other)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

pub fn read_server_json(data_dir: &Path) -> Option<ServerInfo> {
    let contents = fs::read_to_string(server_json_path(data_dir)).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn remove_server_json(data_dir: &Path) {
    let _ = fs::remove_file(server_json_path(data_dir));
}

/// Best-effort liveness check for a PID (Windows and Unix). Does not
/// distinguish "alive but a different process reused the PID" -- callers
/// pair this with an HTTP `/health` probe for a stronger signal.
#[cfg(windows)]
pub fn pid_is_alive(pid: u32) -> bool {
    use std::process::Command;
    // `tasklist /FI "PID eq <pid>"` prints a header plus a matching row, or
    // just the header (and "No tasks...") when absent.
    match Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Try to acquire the exclusive single-instance lock for `data_dir`. Returns
/// the open (locked) file handle on success -- keep it alive for the process
/// lifetime, its drop releases the lock. `Err` means another live instance
/// already holds it.
pub fn acquire_lock(data_dir: &Path) -> std::io::Result<File> {
    fs::create_dir_all(data_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(data_dir))?;
    file.try_lock()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::WouldBlock, "server.lock is held"))?;
    Ok(file)
}

/// Whether `host` is a loopback address (IPv4 127.0.0.0/8 or IPv6 ::1).
/// Anything else -- including `0.0.0.0`, `::`, or a LAN address -- is
/// considered network-reachable and triggers the LAN token guard.
pub fn is_loopback_host(host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("stt-discovery-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_back_server_json() {
        let dir = temp_dir();
        let info = ServerInfo::new("127.0.0.1".to_owned(), 54321);
        write_server_json(&dir, &info).unwrap();
        let read = read_server_json(&dir).unwrap();
        assert_eq!(read, info);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_server_json_reads_as_none() {
        let dir = temp_dir();
        assert!(read_server_json(&dir).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_pid_is_detected_not_alive() {
        // A very large PID is extremely unlikely to be alive; this is the
        // "fake PID" stand-in the task calls for. (PID 0 is not usable here:
        // on Windows it names the real "System Idle Process".)
        assert!(!pid_is_alive(999_999_999));
    }

    #[test]
    fn second_lock_on_same_dir_fails() {
        let dir = temp_dir();
        let _first = acquire_lock(&dir).unwrap();
        let second = acquire_lock(&dir);
        assert!(second.is_err());
        drop(_first);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_is_released_on_drop_so_a_new_instance_can_acquire_it() {
        let dir = temp_dir();
        {
            let _first = acquire_lock(&dir).unwrap();
        }
        let second = acquire_lock(&dir);
        assert!(second.is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("::"));
        assert!(!is_loopback_host("192.168.1.50"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("not-an-ip"));
    }
}
