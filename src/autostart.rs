//! Per-user "start with Windows" autostart, no admin required: a single
//! value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Uses the
//! built-in `reg.exe` (already the pattern `service.rs` uses for `icacls`)
//! rather than adding a registry-access crate dependency.

use std::error::Error;
use std::path::Path;
use std::process::Command;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "OpenVibeSttServer";

/// Builds the command line stored in the registry value: the exe path
/// (quoted), `start`, and `--data-dir` plus any given `--port`/`--host`.
pub fn command_line(exe: &Path, data_dir: &Path, flags: &crate::cli::RunFlags) -> String {
    std::iter::once(exe.display().to_string())
        .chain(std::iter::once("start".to_owned()))
        .chain(flags.arguments(data_dir))
        .map(|arg| crate::update_transaction::quote_windows_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn enable(
    exe: &Path,
    data_dir: &Path,
    flags: &crate::cli::RunFlags,
) -> Result<(), Box<dyn Error>> {
    let value = command_line(exe, data_dir, flags);
    let status = Command::new("reg")
        .args([
            "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
        ])
        .status()?;
    if !status.success() {
        return Err("reg add failed".into());
    }
    Ok(())
}

pub fn disable() -> Result<(), Box<dyn Error>> {
    let status = Command::new("reg")
        .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
        .status()?;
    // `reg delete` on a missing value exits non-zero; treat "already absent"
    // as success (disable is idempotent).
    if !status.success() && status_value_exists()? {
        return Err("reg delete failed".into());
    }
    Ok(())
}

fn status_value_exists() -> Result<bool, Box<dyn Error>> {
    Ok(query()?.is_some())
}

/// Returns the stored command line if the autostart value is present.
pub fn query() -> Result<Option<String>, Box<dyn Error>> {
    let output = Command::new("reg")
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // A line like: "    OpenVibeSttServer    REG_SZ    <value>"
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(VALUE_NAME) {
            if let Some(value) = rest.trim_start().strip_prefix("REG_SZ") {
                return Ok(Some(value.trim().to_owned()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_includes_start_and_data_dir() {
        let line = command_line(
            Path::new(r"C:\bin\stt.exe"),
            Path::new(r"C:\data"),
            &crate::cli::RunFlags::default(),
        );
        assert_eq!(line, r#""C:\bin\stt.exe" "start" "--data-dir" "C:\data""#);
    }

    #[test]
    fn command_line_includes_port_and_host_when_given() {
        let line = command_line(
            Path::new(r"C:\bin\stt.exe"),
            Path::new(r"C:\data"),
            &crate::cli::RunFlags {
                port: Some(54400),
                host: Some("0.0.0.0".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            line,
            r#""C:\bin\stt.exe" "start" "--data-dir" "C:\data" "--port" "54400" "--host" "0.0.0.0""#
        );
    }

    // Regression: autostart's registry `Run` value must carry the explicit
    // `--network` mode through, otherwise a LAN/Tailscale-configured server
    // silently reverts to loopback-only on the next login.
    #[test]
    fn command_line_includes_network_when_given() {
        let line = command_line(
            Path::new(r"C:\bin\stt.exe"),
            Path::new(r"C:\data"),
            &crate::cli::RunFlags {
                network: Some(crate::network::NetworkMode::Lan),
                ..Default::default()
            },
        );
        assert_eq!(
            line,
            r#""C:\bin\stt.exe" "start" "--data-dir" "C:\data" "--network" "lan""#
        );
    }

    #[test]
    fn command_line_includes_cors_origins_when_given() {
        let line = command_line(
            Path::new(r"C:\bin\stt.exe"),
            Path::new(r"C:\data"),
            &crate::cli::RunFlags {
                cors_origins: vec!["https://a.example".to_owned()],
                ..Default::default()
            },
        );
        assert_eq!(
            line,
            r#""C:\bin\stt.exe" "start" "--data-dir" "C:\data" "--cors-origin" "https://a.example""#
        );
    }

    #[test]
    fn parses_reg_query_output() {
        let sample = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    OpenVibeSttServer    REG_SZ    \"C:\\bin\\stt.exe\" start\r\n\r\n";
        let mut found = None;
        for line in sample.lines() {
            if let Some(rest) = line.trim_start().strip_prefix(VALUE_NAME) {
                if let Some(value) = rest.trim_start().strip_prefix("REG_SZ") {
                    found = Some(value.trim().to_owned());
                }
            }
        }
        assert_eq!(found.as_deref(), Some("\"C:\\bin\\stt.exe\" start"));
    }
}
