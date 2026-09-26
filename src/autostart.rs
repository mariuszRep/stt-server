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
pub fn command_line(exe: &Path, data_dir: &Path, port: Option<u16>, host: Option<&str>) -> String {
    let mut line = format!(
        "\"{}\" start --data-dir \"{}\"",
        exe.display(),
        data_dir.display()
    );
    if let Some(port) = port {
        line.push_str(&format!(" --port {port}"));
    }
    if let Some(host) = host {
        line.push_str(&format!(" --host {host}"));
    }
    line
}

pub fn enable(
    exe: &Path,
    data_dir: &Path,
    port: Option<u16>,
    host: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let value = command_line(exe, data_dir, port, host);
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
            None,
            None,
        );
        assert_eq!(line, r#""C:\bin\stt.exe" start --data-dir "C:\data""#);
    }

    #[test]
    fn command_line_includes_port_and_host_when_given() {
        let line = command_line(
            Path::new(r"C:\bin\stt.exe"),
            Path::new(r"C:\data"),
            Some(54400),
            Some("0.0.0.0"),
        );
        assert_eq!(
            line,
            r#""C:\bin\stt.exe" start --data-dir "C:\data" --port 54400 --host 0.0.0.0"#
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
