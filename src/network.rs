//! Network mode resolution and reachability detection for the "Network
//! modes" slice of the `install-scope-and-shared-access` goal.
//!
//! Three modes, settable via `PATCH /v1/local/config` (`network_mode`) or the
//! CLI `--network local|lan|tailscale` flag (CLI > stored setting > default
//! `local`):
//!
//! - `local`: bind loopback only.
//! - `lan`: reachable from other devices only while Windows reports every
//!   active network connection profile as Private or DomainAuthenticated. A
//!   Public profile (or a detection failure) falls back to local-only.
//! - `tailscale`: reachable only through this PC's Tailscale address. If
//!   Tailscale has no address, falls back to local-only.
//!
//! An explicit `--host` override (existing "advanced override", predating
//! this module) always wins over `network_mode` and is reported as mode
//! `"custom"` -- see `crate::app`/`crate::api::run_http_full`.
//!
//! Detection shells out to `powershell`/`tailscale` with a hard timeout so a
//! hung subprocess can never hang server startup or the periodic recheck.
//! Every parsing/decision function here is pure and unit-tested without
//! touching real network state; the process-spawning wrappers are thin and
//! not exercised by tests (no real Tailscale/network changes required).

use std::net::Ipv4Addr;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The user-settable network mode. Does not include the "custom" (explicit
/// `--host`) case, which is tracked separately as `App::network_custom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Local,
    Lan,
    Tailscale,
}

impl NetworkMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(NetworkMode::Local),
            "lan" => Some(NetworkMode::Lan),
            "tailscale" => Some(NetworkMode::Tailscale),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::Local => "local",
            NetworkMode::Lan => "lan",
            NetworkMode::Tailscale => "tailscale",
        }
    }
}

pub const DEFAULT_NETWORK_MODE: NetworkMode = NetworkMode::Local;

/// Precedence: CLI flag > stored setting > default (`local`).
pub fn resolve_mode(cli: Option<NetworkMode>, stored: Option<NetworkMode>) -> NetworkMode {
    cli.or(stored).unwrap_or(DEFAULT_NETWORK_MODE)
}

/// Live network report, exposed on `/health` and refreshed by the periodic
/// recheck task while a `lan`/`tailscale` server is running.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkReport {
    /// The requested mode as reported to clients: `"local"`, `"lan"`,
    /// `"tailscale"`, or `"custom"` (explicit `--host` override).
    pub mode: String,
    /// What's actually in effect right now: `"local"`, `"lan"`, `"tailscale"`,
    /// or `"custom"`.
    pub effective: String,
    /// Why `effective` differs from `mode` (e.g. "network is public",
    /// "tailscale is not running"). `None` when they match.
    pub reason: Option<String>,
    /// Non-loopback addresses actually reachable under the current mode.
    /// Never includes the token.
    pub addresses: Vec<String>,
}

impl NetworkReport {
    pub fn custom(host: &str) -> Self {
        let addresses = if crate::discovery::is_loopback_host(host) {
            Vec::new()
        } else {
            vec![host.to_owned()]
        };
        NetworkReport {
            mode: "custom".to_owned(),
            effective: "custom".to_owned(),
            reason: None,
            addresses,
        }
    }

    pub fn local(mode: NetworkMode) -> Self {
        NetworkReport {
            mode: mode.as_str().to_owned(),
            effective: "local".to_owned(),
            reason: None,
            addresses: Vec::new(),
        }
    }

    pub fn local_fallback(mode: NetworkMode, reason: impl Into<String>) -> Self {
        NetworkReport {
            mode: mode.as_str().to_owned(),
            effective: "local".to_owned(),
            reason: Some(reason.into()),
            addresses: Vec::new(),
        }
    }

    pub fn lan(addresses: Vec<String>) -> Self {
        NetworkReport {
            mode: NetworkMode::Lan.as_str().to_owned(),
            effective: "lan".to_owned(),
            reason: None,
            addresses,
        }
    }

    pub fn tailscale(address: Ipv4Addr) -> Self {
        NetworkReport {
            mode: NetworkMode::Tailscale.as_str().to_owned(),
            effective: "tailscale".to_owned(),
            reason: None,
            addresses: vec![address.to_string()],
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("mode".to_owned(), serde_json::json!(self.mode));
        obj.insert("effective".to_owned(), serde_json::json!(self.effective));
        if let Some(reason) = &self.reason {
            obj.insert("reason".to_owned(), serde_json::json!(reason));
        }
        if !self.addresses.is_empty() {
            obj.insert("addresses".to_owned(), serde_json::json!(self.addresses));
        }
        serde_json::Value::Object(obj)
    }
}

/// Whether a non-loopback caller should be let through, given the resolved
/// mode/custom flag and the live report. `/health` is always allowed
/// (checked by the caller before reaching this) and a loopback peer is
/// always allowed (also checked by the caller). This function only decides
/// the remaining case: a non-loopback peer under an enforced mode.
///
/// - `custom`: the operator explicitly chose a bind host via `--host`; the
///   existing LAN-guard token requirement is the only gate, unchanged.
/// - `local`: never reachable non-loopback (defense in depth; the listener
///   itself is loopback-only in this mode, so this should not be reached).
/// - `lan`: allowed only while the live report's effective mode is `"lan"`
///   (i.e. Windows currently reports every active connection profile as
///   Private/DomainAuthenticated).
/// - `tailscale`: allowed only while the live report's effective mode is
///   `"tailscale"` *and* the peer's own address is itself a Tailscale CGNAT
///   address (`100.64.0.0/10`). A connection that arrives over the Tailscale
///   virtual interface carries the caller's Tailscale address as its source
///   address, so this peer-address check is equivalent to having bound only
///   the Tailscale interface -- without needing a second listener or
///   dynamic rebinding when the interface appears/disappears (see
///   `docs/client-contract.md` "Network modes" for the fuller justification).
pub fn peer_allowed_non_loopback(
    mode: NetworkMode,
    custom: bool,
    report: &NetworkReport,
    peer_ip: std::net::IpAddr,
) -> bool {
    if custom {
        return true;
    }
    match mode {
        NetworkMode::Local => false,
        NetworkMode::Lan => report.effective == "lan",
        NetworkMode::Tailscale => {
            report.effective == "tailscale"
                && match peer_ip {
                    std::net::IpAddr::V4(v4) => is_tailscale_cgnat_ip(v4),
                    std::net::IpAddr::V6(_) => false,
                }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows network connection profile (LAN mode)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkCategory {
    Public,
    Private,
    DomainAuthenticated,
}

impl NetworkCategory {
    fn parse(token: &str) -> Option<Self> {
        match token.trim() {
            "Public" => Some(NetworkCategory::Public),
            "Private" => Some(NetworkCategory::Private),
            "DomainAuthenticated" => Some(NetworkCategory::DomainAuthenticated),
            _ => None,
        }
    }

    fn is_private_enough(self) -> bool {
        matches!(
            self,
            NetworkCategory::Private | NetworkCategory::DomainAuthenticated
        )
    }
}

/// Parses the line-per-profile output of
/// `(Get-NetConnectionProfile).NetworkCategory` (one line per active
/// network adapter's connection profile). Unrecognized/blank lines are
/// skipped rather than treated as an error, so stray PowerShell banner text
/// doesn't spuriously fail detection.
pub fn parse_profile_categories(output: &str) -> Vec<NetworkCategory> {
    output.lines().filter_map(NetworkCategory::parse).collect()
}

/// True only when there is at least one active profile and every one of
/// them is Private or DomainAuthenticated. Empty (no active profile found)
/// or any Public profile in the mix is *not* private -- mixed adapters
/// (e.g. a Private Wi-Fi plus a Public Ethernet) fall back to local-only,
/// matching the goal's "if any active profile is Public ... fall back".
pub fn all_profiles_private(categories: &[NetworkCategory]) -> bool {
    !categories.is_empty()
        && categories
            .iter()
            .all(|category| category.is_private_enough())
}

// ---------------------------------------------------------------------------
// Tailscale address (Tailscale mode)
// ---------------------------------------------------------------------------

/// `100.64.0.0/10`: the CGNAT range Tailscale assigns tailnet addresses
/// from.
pub fn is_tailscale_cgnat_ip(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000
}

/// Parses `tailscale ip -4` output: one address per line (a host can have
/// more than one Tailscale address, e.g. with tailnet lock or subnet
/// routing extras); the first one in the CGNAT range wins.
pub fn parse_tailscale_ip_output(output: &str) -> Option<Ipv4Addr> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .find(|ip| is_tailscale_cgnat_ip(*ip))
}

// ---------------------------------------------------------------------------
// Subprocess plumbing: never hangs, always bounded by `timeout`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectError {
    /// The subprocess did not finish within the timeout; it was killed.
    Timeout,
    /// The subprocess could not be spawned, or exited with a failure status.
    Failed,
}

/// Runs `command`, polling `try_wait` rather than blocking on `wait()` so a
/// hung subprocess (e.g. PowerShell prompting, or `tailscale` blocked on a
/// daemon that's wedged) can be killed instead of hanging the caller. Never
/// blocks longer than `timeout`.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, DetectError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| DetectError::Failed)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().map_err(|_| DetectError::Failed);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DetectError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return Err(DetectError::Failed),
        }
    }
}

/// Queries Windows for the active network connection profile(s)' category.
/// Real Windows/PowerShell only -- not exercised by tests (see module docs).
pub fn query_network_profile_categories(
    timeout: Duration,
) -> Result<Vec<NetworkCategory>, DetectError> {
    let mut command = Command::new("powershell");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "(Get-NetConnectionProfile).NetworkCategory",
    ]);
    let output = run_with_timeout(command, timeout)?;
    if !output.status.success() {
        return Err(DetectError::Failed);
    }
    Ok(parse_profile_categories(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// LAN-mode detection: `Ok(true)` when it's safe to accept LAN callers right
/// now, `Ok(false)`/`Err` (both treated the same by the caller -- fall back
/// to local-only) otherwise.
pub fn detect_lan_private(timeout: Duration) -> bool {
    match query_network_profile_categories(timeout) {
        Ok(categories) => all_profiles_private(&categories),
        Err(_) => false,
    }
}

/// Queries the Tailscale CLI for this host's Tailscale IPv4 address. Real
/// `tailscale` binary only -- not exercised by tests.
pub fn query_tailscale_ipv4(timeout: Duration) -> Result<Ipv4Addr, DetectError> {
    let mut command = Command::new("tailscale");
    command.args(["ip", "-4"]);
    let output = run_with_timeout(command, timeout)?;
    if !output.status.success() {
        return Err(DetectError::Failed);
    }
    parse_tailscale_ip_output(&String::from_utf8_lossy(&output.stdout)).ok_or(DetectError::Failed)
}

/// Tailscale-mode detection: `Some(ip)` when Tailscale is up and has a
/// CGNAT address, `None` (fall back to local-only) otherwise.
pub fn detect_tailscale_ipv4(timeout: Duration) -> Option<Ipv4Addr> {
    query_tailscale_ipv4(timeout).ok()
}

/// How often the periodic recheck runs while a `lan`/`tailscale` server is
/// serving.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Timeout for a single detection subprocess call -- generous enough for a
/// cold PowerShell/`tailscale.exe` start, short enough to never meaningfully
/// delay startup or a recheck tick.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_roundtrips_the_three_values() {
        assert_eq!(NetworkMode::parse("local"), Some(NetworkMode::Local));
        assert_eq!(NetworkMode::parse("lan"), Some(NetworkMode::Lan));
        assert_eq!(
            NetworkMode::parse("tailscale"),
            Some(NetworkMode::Tailscale)
        );
        assert_eq!(NetworkMode::parse("LAN"), None);
        assert_eq!(NetworkMode::parse(""), None);
        assert_eq!(NetworkMode::parse("custom"), None);
    }

    #[test]
    fn resolve_mode_precedence_is_cli_then_stored_then_default() {
        assert_eq!(
            resolve_mode(Some(NetworkMode::Lan), Some(NetworkMode::Tailscale)),
            NetworkMode::Lan
        );
        assert_eq!(
            resolve_mode(None, Some(NetworkMode::Tailscale)),
            NetworkMode::Tailscale
        );
        assert_eq!(resolve_mode(None, None), NetworkMode::Local);
    }

    #[test]
    fn parses_a_single_private_profile() {
        let categories = parse_profile_categories("Private\n");
        assert_eq!(categories, vec![NetworkCategory::Private]);
        assert!(all_profiles_private(&categories));
    }

    #[test]
    fn parses_a_single_public_profile() {
        let categories = parse_profile_categories("Public\r\n");
        assert_eq!(categories, vec![NetworkCategory::Public]);
        assert!(!all_profiles_private(&categories));
    }

    #[test]
    fn parses_domain_authenticated_as_private_enough() {
        let categories = parse_profile_categories("DomainAuthenticated\n");
        assert!(all_profiles_private(&categories));
    }

    #[test]
    fn mixed_profiles_with_any_public_are_not_private() {
        let categories = parse_profile_categories("Private\nPublic\n");
        assert_eq!(
            categories,
            vec![NetworkCategory::Private, NetworkCategory::Public]
        );
        assert!(!all_profiles_private(&categories));
    }

    #[test]
    fn mixed_private_and_domain_authenticated_are_private() {
        let categories = parse_profile_categories("Private\nDomainAuthenticated\n");
        assert!(all_profiles_private(&categories));
    }

    #[test]
    fn no_active_profile_lines_is_not_private() {
        assert!(!all_profiles_private(&parse_profile_categories("")));
        assert!(!all_profiles_private(&parse_profile_categories(
            "\n\n   \n"
        )));
    }

    #[test]
    fn unrecognized_lines_are_skipped_not_fatal() {
        let categories = parse_profile_categories("some banner text\nPrivate\n");
        assert_eq!(categories, vec![NetworkCategory::Private]);
        assert!(all_profiles_private(&categories));
    }

    #[test]
    fn cgnat_range_boundaries() {
        assert!(is_tailscale_cgnat_ip(Ipv4Addr::new(100, 64, 0, 0)));
        assert!(is_tailscale_cgnat_ip(Ipv4Addr::new(100, 100, 1, 1)));
        assert!(is_tailscale_cgnat_ip(Ipv4Addr::new(100, 127, 255, 255)));
        assert!(!is_tailscale_cgnat_ip(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailscale_cgnat_ip(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(!is_tailscale_cgnat_ip(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(!is_tailscale_cgnat_ip(Ipv4Addr::new(192, 168, 1, 50)));
    }

    #[test]
    fn parses_tailscale_ip_output_picking_the_cgnat_line() {
        assert_eq!(
            parse_tailscale_ip_output("100.101.102.103\n"),
            Some(Ipv4Addr::new(100, 101, 102, 103))
        );
    }

    #[test]
    fn parses_tailscale_ip_output_with_extra_non_cgnat_lines() {
        // A tailnet with e.g. a MagicDNS-less extra address on another
        // range; only the CGNAT line counts.
        assert_eq!(
            parse_tailscale_ip_output("192.168.9.1\n100.64.1.2\n"),
            Some(Ipv4Addr::new(100, 64, 1, 2))
        );
    }

    #[test]
    fn parse_tailscale_ip_output_empty_or_unparseable_is_none() {
        assert_eq!(parse_tailscale_ip_output(""), None);
        assert_eq!(parse_tailscale_ip_output("not-an-ip\n"), None);
    }

    #[test]
    fn peer_allowed_custom_always_true() {
        let report = NetworkReport::local(NetworkMode::Local);
        assert!(peer_allowed_non_loopback(
            NetworkMode::Local,
            true,
            &report,
            "203.0.113.5".parse().unwrap()
        ));
    }

    #[test]
    fn peer_allowed_local_mode_never_allows_non_loopback() {
        let report = NetworkReport::local(NetworkMode::Local);
        assert!(!peer_allowed_non_loopback(
            NetworkMode::Local,
            false,
            &report,
            "192.168.1.5".parse().unwrap()
        ));
    }

    #[test]
    fn peer_allowed_lan_mode_follows_the_live_effective_report() {
        let private = NetworkReport::lan(vec!["192.168.1.10".to_owned()]);
        assert!(peer_allowed_non_loopback(
            NetworkMode::Lan,
            false,
            &private,
            "192.168.1.20".parse().unwrap()
        ));
        let fallen_back = NetworkReport::local_fallback(NetworkMode::Lan, "network is public");
        assert!(!peer_allowed_non_loopback(
            NetworkMode::Lan,
            false,
            &fallen_back,
            "192.168.1.20".parse().unwrap()
        ));
    }

    #[test]
    fn peer_allowed_tailscale_mode_requires_both_live_effective_and_cgnat_peer() {
        let up = NetworkReport::tailscale(Ipv4Addr::new(100, 64, 1, 1));
        assert!(peer_allowed_non_loopback(
            NetworkMode::Tailscale,
            false,
            &up,
            "100.64.1.2".parse().unwrap()
        ));
        // Same "up" state, but the caller's own address is a LAN address,
        // not a Tailscale one -- e.g. someone on plain Wi-Fi trying to reach
        // the machine's Ethernet/Wi-Fi IP directly, which tailscale mode
        // must not allow.
        assert!(!peer_allowed_non_loopback(
            NetworkMode::Tailscale,
            false,
            &up,
            "192.168.1.20".parse().unwrap()
        ));
        let down =
            NetworkReport::local_fallback(NetworkMode::Tailscale, "tailscale is not running");
        assert!(!peer_allowed_non_loopback(
            NetworkMode::Tailscale,
            false,
            &down,
            "100.64.1.2".parse().unwrap()
        ));
    }

    #[test]
    fn report_to_json_omits_absent_reason_and_addresses() {
        let report = NetworkReport::local(NetworkMode::Local);
        let json = report.to_json();
        assert_eq!(json["mode"], "local");
        assert_eq!(json["effective"], "local");
        assert!(json.get("reason").is_none());
        assert!(json.get("addresses").is_none());
    }

    #[test]
    fn report_to_json_includes_reason_and_addresses_when_present() {
        let report = NetworkReport::lan(vec!["192.168.1.10".to_owned()]);
        let json = report.to_json();
        assert_eq!(json["addresses"][0], "192.168.1.10");
        let fallback = NetworkReport::local_fallback(NetworkMode::Lan, "network is public");
        let json = fallback.to_json();
        assert_eq!(json["reason"], "network is public");
    }

    #[test]
    fn custom_report_reports_the_explicit_host_as_its_only_address() {
        let report = NetworkReport::custom("0.0.0.0");
        assert_eq!(report.mode, "custom");
        assert_eq!(report.addresses, vec!["0.0.0.0".to_owned()]);
        let loopback = NetworkReport::custom("127.0.0.1");
        assert!(loopback.addresses.is_empty());
    }
}
