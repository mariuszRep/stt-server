//! Hand-parsed CLI: no argument-parsing dependency. See `AGENTS.md`/CLI spec
//! for the full command surface. `parse` is pure (no I/O), so every branch is
//! covered by ordinary unit tests without touching a real process or port.

use std::path::PathBuf;

use crate::app::RuntimeLimits;

pub const USAGE: &str = "\
stt-server-next -- local batch STT server

USAGE:
    stt-server-next [run] [flags]           foreground (default when no command given)
    stt-server-next start [flags]           detached background process
    stt-server-next stop                    graceful stop of the running instance
    stt-server-next restart [flags]         stop then start
    stt-server-next status [--json]         running?, pid, host:port, data dir, version
    stt-server-next autostart enable [flags]   per-user \"start with Windows\"
    stt-server-next autostart disable
    stt-server-next autostart status
    stt-server-next service install|uninstall|run
    stt-server-next --help

FLAGS:
    --port <n>                    bind port (default 54321)
    --host <addr>                 bind host (default 127.0.0.1)
    --data-dir <path>              data directory
    --queue-max-waiting <n>
    --queue-wait-timeout-ms <n>
    --inference-timeout-ms <n>
";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunFlags {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub limits: RuntimeLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Install,
    Uninstall,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartAction {
    Enable,
    Disable,
    Status,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Run(RunFlags),
    Start(RunFlags),
    Stop {
        data_dir: Option<PathBuf>,
    },
    Restart(RunFlags),
    Status {
        json: bool,
        data_dir: Option<PathBuf>,
    },
    Autostart {
        action: AutostartAction,
        flags: RunFlags,
    },
    Service(ServiceAction),
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CliError {
    pub message: String,
    /// Exit code the binary should use: 2 for a usage error.
    pub exit_code: i32,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        CliError {
            message: message.into(),
            exit_code: 2,
        }
    }
}

fn next_value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, CliError> {
    iter.next()
        .cloned()
        .ok_or_else(|| CliError::usage(format!("{flag} requires a value")))
}

fn parse_positive_u64(flag: &str, value: &str) -> Result<u64, CliError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| {
            CliError::usage(format!("{flag} requires a positive integer, got '{value}'"))
        })
}

/// Parses the shared run/start/restart/autostart-enable flag set:
/// `--port`, `--host`, `--data-dir`, and the three queue/inference limit
/// flags. Unknown flags or bad values are a usage error (exit code 2).
pub fn parse_run_flags(args: &[String]) -> Result<RunFlags, CliError> {
    let mut flags = RunFlags::default();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--port" => {
                let raw = next_value(&mut iter, flag)?;
                let port = raw.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| {
                    CliError::usage(format!("--port requires a 1-65535 integer, got '{raw}'"))
                })?;
                flags.port = Some(port);
            }
            "--host" => {
                flags.host = Some(next_value(&mut iter, flag)?);
            }
            "--data-dir" => {
                flags.data_dir = Some(PathBuf::from(next_value(&mut iter, flag)?));
            }
            "--queue-max-waiting" => {
                let raw = next_value(&mut iter, flag)?;
                flags.limits.queue_max_waiting = Some(parse_positive_u64(flag, &raw)? as usize);
            }
            "--queue-wait-timeout-ms" => {
                let raw = next_value(&mut iter, flag)?;
                flags.limits.queue_wait_timeout_ms = Some(parse_positive_u64(flag, &raw)?);
            }
            "--inference-timeout-ms" => {
                let raw = next_value(&mut iter, flag)?;
                flags.limits.inference_timeout_ms = Some(parse_positive_u64(flag, &raw)?);
            }
            other => return Err(CliError::usage(format!("unknown flag: {other}"))),
        }
    }
    Ok(flags)
}

fn parse_status_flags(args: &[String]) -> Result<(bool, Option<PathBuf>), CliError> {
    let mut json = false;
    let mut data_dir = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--json" => json = true,
            "--data-dir" => data_dir = Some(PathBuf::from(next_value(&mut iter, flag)?)),
            other => return Err(CliError::usage(format!("unknown flag: {other}"))),
        }
    }
    Ok((json, data_dir))
}

fn parse_data_dir_only(args: &[String]) -> Result<Option<PathBuf>, CliError> {
    let mut data_dir = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(next_value(&mut iter, flag)?)),
            other => return Err(CliError::usage(format!("unknown flag: {other}"))),
        }
    }
    Ok(data_dir)
}

fn parse_no_flags(args: &[String], command: &str) -> Result<(), CliError> {
    if let Some(first) = args.first() {
        return Err(CliError::usage(format!(
            "{command} takes no arguments, got '{first}'"
        )));
    }
    Ok(())
}

/// Parses the full argument vector (already stripped of argv[0]). Backward
/// compatible: no command at all, or the first token itself looking like a
/// flag (`--...`), means "run" with those flags -- matching the previous
/// (pre-CLI) default-run behavior.
pub fn parse(args: &[String]) -> Result<Command, CliError> {
    let Some(first) = args.first() else {
        return Ok(Command::Run(RunFlags::default()));
    };
    if matches!(first.as_str(), "--help" | "-h" | "help") {
        return Ok(Command::Help);
    }
    if first.starts_with("--") {
        return Ok(Command::Run(parse_run_flags(args)?));
    }
    match first.as_str() {
        "run" => Ok(Command::Run(parse_run_flags(&args[1..])?)),
        "start" => Ok(Command::Start(parse_run_flags(&args[1..])?)),
        "stop" => Ok(Command::Stop {
            data_dir: parse_data_dir_only(&args[1..])?,
        }),
        "restart" => Ok(Command::Restart(parse_run_flags(&args[1..])?)),
        "status" => {
            let (json, data_dir) = parse_status_flags(&args[1..])?;
            Ok(Command::Status { json, data_dir })
        }
        "autostart" => {
            let sub = args
                .get(1)
                .ok_or_else(|| CliError::usage("autostart requires enable|disable|status"))?;
            match sub.as_str() {
                "enable" => Ok(Command::Autostart {
                    action: AutostartAction::Enable,
                    flags: parse_run_flags(&args[2..])?,
                }),
                "disable" => {
                    parse_no_flags(&args[2..], "autostart disable")?;
                    Ok(Command::Autostart {
                        action: AutostartAction::Disable,
                        flags: RunFlags::default(),
                    })
                }
                "status" => {
                    parse_no_flags(&args[2..], "autostart status")?;
                    Ok(Command::Autostart {
                        action: AutostartAction::Status,
                        flags: RunFlags::default(),
                    })
                }
                other => Err(CliError::usage(format!(
                    "unknown autostart subcommand: {other}"
                ))),
            }
        }
        // `service`, plus the pre-existing top-level `install`/`uninstall`
        // spellings kept working as aliases for `service install`/`service
        // uninstall`.
        "service" => {
            let sub = args
                .get(1)
                .ok_or_else(|| CliError::usage("service requires install|uninstall|run"))?;
            match sub.as_str() {
                "install" => {
                    parse_no_flags(&args[2..], "service install")?;
                    Ok(Command::Service(ServiceAction::Install))
                }
                "uninstall" => {
                    parse_no_flags(&args[2..], "service uninstall")?;
                    Ok(Command::Service(ServiceAction::Uninstall))
                }
                "run" => {
                    parse_no_flags(&args[2..], "service run")?;
                    Ok(Command::Service(ServiceAction::Run))
                }
                other => Err(CliError::usage(format!(
                    "unknown service subcommand: {other}"
                ))),
            }
        }
        "install" => {
            parse_no_flags(&args[1..], "install")?;
            Ok(Command::Service(ServiceAction::Install))
        }
        "uninstall" => {
            parse_no_flags(&args[1..], "uninstall")?;
            Ok(Command::Service(ServiceAction::Uninstall))
        }
        other => Err(CliError::usage(format!("unknown command: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn no_command_defaults_to_run_with_no_flags() {
        assert_eq!(parse(&[]).unwrap(), Command::Run(RunFlags::default()));
    }

    #[test]
    fn backward_compatible_bare_flags_mean_run() {
        let cmd = parse(&args(&["--queue-max-waiting", "5"])).unwrap();
        match cmd {
            Command::Run(flags) => assert_eq!(flags.limits.queue_max_waiting, Some(5)),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn explicit_run_parses_port_host_data_dir() {
        let cmd = parse(&args(&[
            "run",
            "--port",
            "9999",
            "--host",
            "0.0.0.0",
            "--data-dir",
            "C:\\data",
        ]))
        .unwrap();
        match cmd {
            Command::Run(flags) => {
                assert_eq!(flags.port, Some(9999));
                assert_eq!(flags.host.as_deref(), Some("0.0.0.0"));
                assert_eq!(flags.data_dir, Some(PathBuf::from("C:\\data")));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn start_parses_like_run() {
        let cmd = parse(&args(&["start", "--port", "1234"])).unwrap();
        match cmd {
            Command::Start(flags) => assert_eq!(flags.port, Some(1234)),
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn stop_accepts_only_data_dir() {
        assert_eq!(
            parse(&args(&["stop"])).unwrap(),
            Command::Stop { data_dir: None }
        );
        assert_eq!(
            parse(&args(&["stop", "--data-dir", "C:\\d"])).unwrap(),
            Command::Stop {
                data_dir: Some(PathBuf::from("C:\\d"))
            }
        );
        assert!(parse(&args(&["stop", "--port", "1"])).is_err());
    }

    #[test]
    fn restart_parses_flags() {
        let cmd = parse(&args(&["restart", "--port", "1"])).unwrap();
        match cmd {
            Command::Restart(flags) => assert_eq!(flags.port, Some(1)),
            _ => panic!("expected Restart"),
        }
    }

    #[test]
    fn status_defaults_to_non_json_and_accepts_json_flag() {
        assert_eq!(
            parse(&args(&["status"])).unwrap(),
            Command::Status {
                json: false,
                data_dir: None
            }
        );
        assert_eq!(
            parse(&args(&["status", "--json"])).unwrap(),
            Command::Status {
                json: true,
                data_dir: None
            }
        );
        assert_eq!(
            parse(&args(&["status", "--json", "--data-dir", "C:\\d"])).unwrap(),
            Command::Status {
                json: true,
                data_dir: Some(PathBuf::from("C:\\d"))
            }
        );
    }

    #[test]
    fn autostart_subcommands() {
        assert_eq!(
            parse(&args(&["autostart", "status"])).unwrap(),
            Command::Autostart {
                action: AutostartAction::Status,
                flags: RunFlags::default()
            }
        );
        assert_eq!(
            parse(&args(&["autostart", "disable"])).unwrap(),
            Command::Autostart {
                action: AutostartAction::Disable,
                flags: RunFlags::default()
            }
        );
        let cmd = parse(&args(&["autostart", "enable", "--port", "8080"])).unwrap();
        match cmd {
            Command::Autostart {
                action: AutostartAction::Enable,
                flags,
            } => assert_eq!(flags.port, Some(8080)),
            _ => panic!("expected Autostart Enable"),
        }
        assert!(parse(&args(&["autostart"])).is_err());
        assert!(parse(&args(&["autostart", "bogus"])).is_err());
    }

    #[test]
    fn service_subcommands_and_legacy_aliases() {
        assert_eq!(
            parse(&args(&["service", "install"])).unwrap(),
            Command::Service(ServiceAction::Install)
        );
        assert_eq!(
            parse(&args(&["service", "uninstall"])).unwrap(),
            Command::Service(ServiceAction::Uninstall)
        );
        assert_eq!(
            parse(&args(&["service", "run"])).unwrap(),
            Command::Service(ServiceAction::Run)
        );
        assert_eq!(
            parse(&args(&["install"])).unwrap(),
            Command::Service(ServiceAction::Install)
        );
        assert_eq!(
            parse(&args(&["uninstall"])).unwrap(),
            Command::Service(ServiceAction::Uninstall)
        );
        assert!(parse(&args(&["service"])).is_err());
        assert!(parse(&args(&["service", "bogus"])).is_err());
    }

    #[test]
    fn unknown_command_is_exit_code_2() {
        let error = parse(&args(&["bogus"])).unwrap_err();
        assert_eq!(error.exit_code, 2);
    }

    #[test]
    fn unknown_flag_is_exit_code_2() {
        let error = parse(&args(&["run", "--bogus"])).unwrap_err();
        assert_eq!(error.exit_code, 2);
    }

    #[test]
    fn help_flag_is_recognized() {
        assert_eq!(parse(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse(&args(&["help"])).unwrap(), Command::Help);
        assert_eq!(parse(&args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn rejects_zero_and_too_large_port() {
        assert!(parse(&args(&["run", "--port", "0"])).is_err());
        assert!(parse(&args(&["run", "--port", "70000"])).is_err());
    }

    #[test]
    fn rejects_missing_flag_value() {
        assert!(parse(&args(&["run", "--port"])).is_err());
    }
}
