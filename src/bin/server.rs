use std::error::Error;

use stt_server_next::app::RuntimeLimits;

/// Optional flags for the default (non-`service`/`install`/`uninstall`) run
/// mode: `--queue-max-waiting <n>`, `--queue-wait-timeout-ms <n>`,
/// `--inference-timeout-ms <n>`. Each overrides the corresponding stored
/// setting for this process only. Hand-rolled (no argument-parsing
/// dependency): unknown flags or a non-positive-integer value are a clear
/// error on stderr and exit code 2.
fn parse_limit_flags(args: &[String]) -> Result<RuntimeLimits, String> {
    let mut limits = RuntimeLimits::default();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut positive = |flag: &str| -> Result<u64, String> {
            let value = iter
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))?;
            value
                .parse::<u64>()
                .ok()
                .filter(|parsed| *parsed > 0)
                .ok_or_else(|| format!("{flag} requires a positive integer, got '{value}'"))
        };
        match flag.as_str() {
            "--queue-max-waiting" => {
                limits.queue_max_waiting = Some(positive(flag)? as usize);
            }
            "--queue-wait-timeout-ms" => {
                limits.queue_wait_timeout_ms = Some(positive(flag)?);
            }
            "--inference-timeout-ms" => {
                limits.inference_timeout_ms = Some(positive(flag)?);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(limits)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    match args.first().map(String::as_str) {
        Some("service") => return stt_server_next::service::dispatch(),
        Some("install") => return stt_server_next::service::install(),
        Some("uninstall") => return stt_server_next::service::uninstall(),
        _ => {}
    }
    let limits = match parse_limit_flags(&args) {
        Ok(limits) => limits,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };
    stt_server_next::api::run_http_with_overrides(limits, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn no_flags_leaves_everything_unset() {
        assert_eq!(parse_limit_flags(&[]).unwrap(), RuntimeLimits::default());
    }

    #[test]
    fn parses_all_three_valid_flags() {
        let limits = parse_limit_flags(&args(&[
            "--queue-max-waiting",
            "5",
            "--queue-wait-timeout-ms",
            "2000",
            "--inference-timeout-ms",
            "30000",
        ]))
        .unwrap();
        assert_eq!(
            limits,
            RuntimeLimits {
                queue_max_waiting: Some(5),
                queue_wait_timeout_ms: Some(2000),
                inference_timeout_ms: Some(30000),
            }
        );
    }

    #[test]
    fn rejects_non_numeric_value() {
        let error = parse_limit_flags(&args(&["--queue-max-waiting", "not-a-number"])).unwrap_err();
        assert!(error.contains("--queue-max-waiting"));
    }

    #[test]
    fn rejects_zero_value() {
        let error = parse_limit_flags(&args(&["--inference-timeout-ms", "0"])).unwrap_err();
        assert!(error.contains("positive"));
    }

    #[test]
    fn rejects_missing_value() {
        let error = parse_limit_flags(&args(&["--queue-wait-timeout-ms"])).unwrap_err();
        assert!(error.contains("requires a value"));
    }

    #[test]
    fn rejects_unknown_flag() {
        let error = parse_limit_flags(&args(&["--bogus-flag"])).unwrap_err();
        assert!(error.contains("unknown flag"));
    }
}
