//! Safe self-update: check GitHub Releases for a newer `stt-server-next`,
//! download and verify it before it ever touches the running executable, and
//! replace the running binary in a way that can always be undone.
//!
//! Design (see `.projectflows/goals/*/safe-self-update/GOAL.md`):
//! - The release source is GitHub Releases of this repo. It is private today,
//!   so `default_update_endpoint` can be overridden with `STT_NEXT_UPDATE_URL`
//!   to point at a local/mock server for rehearsal; production defaults to
//!   the real GitHub API "latest release" endpoint.
//! - A release is expected to publish two assets: the executable
//!   (`EXE_ASSET_NAME`) and a sibling text file with its SHA-256 hex digest
//!   (`CHECKSUM_ASSET_NAME`, the same convention as `sha256sum` output: hex
//!   digest, optionally followed by whitespace and a filename).
//! - Nothing here ever overwrites the running executable directly: the new
//!   binary is always fully downloaded and hash-verified into a staging file
//!   first ([`download_and_verify`]), and swapped in only by [`replace_exe`],
//!   which keeps exactly one previous generation as `<exe>.old` so
//!   [`rollback_exe`] can always restore it.
//! - Download only happens when the caller asks (the CLI's `update install`);
//!   `update check` never downloads anything.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::verify::sha256_file;

/// Asset names a release must publish for `stt-server-next` to recognize it.
pub const EXE_ASSET_NAME: &str = "stt-server-next.exe";
pub const CHECKSUM_ASSET_NAME: &str = "stt-server-next.exe.sha256";

/// Default production release source: the GitHub API's "latest release" for
/// this repo. Overridable via `STT_NEXT_UPDATE_URL` so tests (and a rehearsal
/// against a controlled source, per the goal) can point at a local server
/// without touching the network or requiring the repo to be public.
pub fn default_update_endpoint() -> String {
    std::env::var("STT_NEXT_UPDATE_URL").unwrap_or_else(|_| {
        "https://api.github.com/repos/mariuszRep/stt-server-next/releases/latest".to_owned()
    })
}

/// This build's own version, as embedded at compile time.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// The release tag, with any leading `v` stripped (e.g. `"0.2.0"`).
    pub version: String,
    pub exe: ReleaseAsset,
    pub checksum: ReleaseAsset,
}

#[derive(Debug, Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawRelease {
    tag_name: String,
    assets: Vec<RawAsset>,
}

/// Parse a GitHub "release" JSON body (as returned by the `.../releases/latest`
/// endpoint) into a [`ReleaseInfo`], requiring both `EXE_ASSET_NAME` and
/// `CHECKSUM_ASSET_NAME` to be present among its assets. Pure and independent
/// of I/O so it is unit-testable without a network or a mock server.
pub fn parse_release(body: &str) -> Result<ReleaseInfo, String> {
    let raw: RawRelease =
        serde_json::from_str(body).map_err(|error| format!("invalid release JSON: {error}"))?;
    let find = |name: &str| -> Option<ReleaseAsset> {
        raw.assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| ReleaseAsset {
                name: asset.name.clone(),
                url: asset.browser_download_url.clone(),
                size: asset.size,
            })
    };
    let exe = find(EXE_ASSET_NAME)
        .ok_or_else(|| format!("release is missing required asset '{EXE_ASSET_NAME}'"))?;
    let checksum = find(CHECKSUM_ASSET_NAME)
        .ok_or_else(|| format!("release is missing required asset '{CHECKSUM_ASSET_NAME}'"))?;
    let version = raw
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&raw.tag_name)
        .to_owned();
    Ok(ReleaseInfo {
        version,
        exe,
        checksum,
    })
}

/// Parse a numeric dot-separated version (`"1.2.3"` -> `[1,2,3]`) for
/// comparison; components missing on one side compare as `0`.
fn parse_numeric_version(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// Whether `candidate` is a newer version than `current`. Numeric dot
/// versions (the normal case: `"0.1.0"` vs `"0.2.0"`) compare component by
/// component; anything that doesn't parse that way falls back to "different
/// string means newer", so an unexpected version scheme is never silently
/// treated as "already up to date" -- a fresh check on `update install` still
/// double-checks with an explicit re-comparison, so this fallback only ever
/// offers an update, never applies one blindly.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (
        parse_numeric_version(current),
        parse_numeric_version(candidate),
    ) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => current != candidate,
    }
}

/// Result of [`check_latest`]: what's available, and whether it's newer than
/// this running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub current_version: String,
    pub release: ReleaseInfo,
    pub update_available: bool,
}

#[derive(Debug)]
pub enum UpdateError {
    Http(String),
    Parse(String),
    Verify(String),
    Io(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Http(m) => write!(f, "{m}"),
            UpdateError::Parse(m) => write!(f, "{m}"),
            UpdateError::Verify(m) => write!(f, "{m}"),
            UpdateError::Io(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for UpdateError {}

/// GET `endpoint` and parse it as a release. GitHub's API requires a
/// `User-Agent` header on every request (rejects requests without one), and a
/// local mock server for tests tolerates it fine.
pub async fn check_latest(
    http: &reqwest::Client,
    endpoint: &str,
) -> Result<UpdateCheck, UpdateError> {
    let response = http
        .get(endpoint)
        .header(reqwest::header::USER_AGENT, "stt-server-next-self-update")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(UpdateError::Http(format!(
            "release source returned HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    let release = parse_release(&body).map_err(UpdateError::Parse)?;
    let current = current_version().to_owned();
    let update_available = is_newer(&current, &release.version);
    Ok(UpdateCheck {
        current_version: current,
        release,
        update_available,
    })
}

/// Download `url`'s full body into `dest` (overwriting), returning the byte
/// count written.
async fn download_to_file(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<u64, UpdateError> {
    let response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, "stt-server-next-self-update")
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(UpdateError::Http(format!(
            "asset download returned HTTP {} for {url}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| UpdateError::Io(error.to_string()))?;
    }
    let mut file =
        std::fs::File::create(dest).map_err(|error| UpdateError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .map_err(|error| UpdateError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| UpdateError::Io(error.to_string()))?;
    Ok(bytes.len() as u64)
}

/// The first whitespace-separated token of a `sha256sum`-style checksum file
/// body, lower-cased. Pure so the checksum-file format is unit-testable
/// without touching the network.
pub fn parse_checksum_body(body: &str) -> Option<String> {
    body.split_whitespace().next().map(str::to_lowercase)
}

/// Download the release's executable and checksum assets into `stage_dir`,
/// verify the executable's SHA-256 against the checksum asset, and return the
/// path to the verified staged executable. Never touches the running
/// executable -- that only happens in [`replace_exe`], called separately once
/// this succeeds. On any failure (network, parse, or hash mismatch) the
/// staged executable is removed so a failed attempt never leaves an
/// unverified binary lying around.
pub async fn download_and_verify(
    http: &reqwest::Client,
    release: &ReleaseInfo,
    stage_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    let staged_exe = stage_dir.join(format!("stt-server-next-{}.exe", release.version));
    let staged_checksum = stage_dir.join(format!("stt-server-next-{}.exe.sha256", release.version));
    download_to_file(http, &release.exe.url, &staged_exe).await?;
    let checksum_result = download_to_file(http, &release.checksum.url, &staged_checksum).await;
    if let Err(error) = checksum_result {
        let _ = std::fs::remove_file(&staged_exe);
        return Err(error);
    }
    let checksum_body = std::fs::read_to_string(&staged_checksum)
        .map_err(|error| UpdateError::Io(error.to_string()))?;
    let _ = std::fs::remove_file(&staged_checksum);
    let expected = match parse_checksum_body(&checksum_body) {
        Some(value) => value,
        None => {
            let _ = std::fs::remove_file(&staged_exe);
            return Err(UpdateError::Parse(
                "checksum asset was empty or unparseable".to_owned(),
            ));
        }
    };
    let staged_exe_for_hash = staged_exe.clone();
    let actual = tokio::task::spawn_blocking(move || sha256_file(&staged_exe_for_hash))
        .await
        .map_err(|error| UpdateError::Io(error.to_string()))?
        .map_err(|error| UpdateError::Io(error.to_string()))?;
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = std::fs::remove_file(&staged_exe);
        return Err(UpdateError::Verify(format!(
            "downloaded executable's SHA-256 ({actual}) does not match the release's checksum ({expected})"
        )));
    }
    Ok(staged_exe)
}

/// Where [`replace_exe`] keeps the previous generation, next to `current_exe`.
pub fn backup_path(current_exe: &Path) -> PathBuf {
    let mut name = current_exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".old");
    current_exe.with_file_name(name)
}

/// Swap `staged_exe` (already hash-verified by [`download_and_verify`]) into
/// `current_exe`'s place. `current_exe` must not be running/locked when this
/// is called (the CLI stops the server first). Keeps exactly one previous
/// generation at [`backup_path`] so [`rollback_exe`] can always undo this,
/// and only removes an older backup once the new file is safely in place.
/// Returns the backup path.
pub fn replace_exe(current_exe: &Path, staged_exe: &Path) -> Result<PathBuf, String> {
    let backup = backup_path(current_exe);
    if backup.exists() {
        std::fs::remove_file(&backup).map_err(|error| {
            format!(
                "could not remove stale backup {}: {error}",
                backup.display()
            )
        })?;
    }
    if current_exe.exists() {
        std::fs::rename(current_exe, &backup).map_err(|error| {
            format!(
                "could not move running executable aside to {}: {error}",
                backup.display()
            )
        })?;
    }
    // Prefer a rename (atomic, instant); fall back to copy+remove if the
    // staged file lives on a different volume than the executable.
    match std::fs::rename(staged_exe, current_exe) {
        Ok(()) => {}
        Err(_) => {
            std::fs::copy(staged_exe, current_exe).map_err(|error| {
                // Best-effort restore so a failed swap doesn't leave the
                // program missing entirely.
                let _ = std::fs::rename(&backup, current_exe);
                format!("could not install new executable: {error}")
            })?;
            let _ = std::fs::remove_file(staged_exe);
        }
    }
    Ok(backup)
}

/// Undo [`replace_exe`]: restore the previous generation from `backup` back
/// to `current_exe`. `current_exe` must not be running/locked (the CLI stops
/// the new, unhealthy process first).
pub fn rollback_exe(current_exe: &Path, backup: &Path) -> Result<(), String> {
    if !backup.exists() {
        return Err(format!(
            "no backup executable found at {} to roll back to",
            backup.display()
        ));
    }
    if current_exe.exists() {
        std::fs::remove_file(current_exe).map_err(|error| {
            format!(
                "could not remove failed new executable {}: {error}",
                current_exe.display()
            )
        })?;
    }
    std::fs::rename(backup, current_exe).map_err(|error| {
        format!(
            "could not restore previous executable from {}: {error}",
            backup.display()
        )
    })
}

/// Parse a `serde_json::Value` body the CLI already fetched into a
/// [`ReleaseInfo`] -- convenience for callers that want to reuse an
/// already-deserialized response.
pub fn parse_release_value(value: &Value) -> Result<ReleaseInfo, String> {
    parse_release(&value.to_string())
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    fn sample_release_json(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","assets":[
                {{"name":"{EXE_ASSET_NAME}","browser_download_url":"https://example.com/exe","size":123}},
                {{"name":"{CHECKSUM_ASSET_NAME}","browser_download_url":"https://example.com/sha"}}
            ]}}"#
        )
    }

    #[test]
    fn parse_release_extracts_version_and_assets() {
        let release = parse_release(&sample_release_json("v0.2.0")).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.exe.url, "https://example.com/exe");
        assert_eq!(release.exe.size, Some(123));
        assert_eq!(release.checksum.url, "https://example.com/sha");
    }

    #[test]
    fn parse_release_tolerates_missing_v_prefix() {
        let release = parse_release(&sample_release_json("0.2.0")).unwrap();
        assert_eq!(release.version, "0.2.0");
    }

    #[test]
    fn parse_release_requires_exe_asset() {
        let body = format!(
            r#"{{"tag_name":"v0.2.0","assets":[{{"name":"{CHECKSUM_ASSET_NAME}","browser_download_url":"https://example.com/sha"}}]}}"#
        );
        let error = parse_release(&body).unwrap_err();
        assert!(error.contains(EXE_ASSET_NAME));
    }

    #[test]
    fn parse_release_requires_checksum_asset() {
        let body = format!(
            r#"{{"tag_name":"v0.2.0","assets":[{{"name":"{EXE_ASSET_NAME}","browser_download_url":"https://example.com/exe"}}]}}"#
        );
        let error = parse_release(&body).unwrap_err();
        assert!(error.contains(CHECKSUM_ASSET_NAME));
    }

    #[test]
    fn parse_release_rejects_garbage() {
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn is_newer_compares_numeric_versions() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn is_newer_falls_back_to_string_inequality_for_odd_schemes() {
        assert!(is_newer("abc", "def"));
        assert!(!is_newer("abc", "abc"));
    }

    #[test]
    fn parse_checksum_body_takes_first_token_lowercased() {
        assert_eq!(
            parse_checksum_body("DEADBEEF  stt-server-next.exe\n"),
            Some("deadbeef".to_owned())
        );
        assert_eq!(parse_checksum_body("   \n"), None);
    }

    #[test]
    fn backup_path_appends_old_suffix() {
        let exe = PathBuf::from(r"C:\Program Files\OpenVibeAI\stt-server-next.exe");
        assert_eq!(
            backup_path(&exe),
            PathBuf::from(r"C:\Program Files\OpenVibeAI\stt-server-next.exe.old")
        );
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "stt-server-next-selfupdate-test-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn replace_then_rollback_round_trips_file_contents() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("stt-server-next.exe");
        let staged = dir.join("staged.exe");
        std::fs::write(&current, b"old version bytes").unwrap();
        std::fs::write(&staged, b"new version bytes").unwrap();

        let backup = replace_exe(&current, &staged).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new version bytes");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old version bytes");
        assert!(!staged.exists());

        rollback_exe(&current, &backup).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old version bytes");
        assert!(!backup.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replace_exe_overwrites_a_stale_prior_backup() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("stt-server-next.exe");
        let staged = dir.join("staged.exe");
        let stale_backup = backup_path(&current);
        std::fs::write(&current, b"gen2").unwrap();
        std::fs::write(&stale_backup, b"gen0-stale").unwrap();
        std::fs::write(&staged, b"gen3").unwrap();

        let backup = replace_exe(&current, &staged).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), b"gen2");
        assert_eq!(std::fs::read(&current).unwrap(), b"gen3");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_without_a_backup_fails_clearly() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("stt-server-next.exe");
        std::fs::write(&current, b"only version").unwrap();
        let missing_backup = dir.join("nope.old");
        let error = rollback_exe(&current, &missing_backup).unwrap_err();
        assert!(error.contains("no backup"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::{extract::State as AxumState, response::IntoResponse, routing::get, Router};
    use sha2::{Digest, Sha256};

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "stt-server-next-selfupdate-http-test-{}",
            uuid::Uuid::new_v4()
        ))
    }

    /// Serves a fake GitHub-releases-shaped `/latest`, plus the exe and
    /// checksum assets it references, all on one ephemeral local port -- the
    /// "controlled local release source" the goal calls for since the real
    /// repo is still private.
    async fn serve_release(tag: &str, exe_bytes: &'static [u8], checksum_body: String) -> String {
        #[derive(Clone)]
        struct Fixture {
            tag: String,
            exe_bytes: &'static [u8],
            checksum_body: String,
            exe_url: String,
            checksum_url: String,
        }

        async fn latest(AxumState(fixture): AxumState<Fixture>) -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "tag_name": fixture.tag,
                "assets": [
                    {"name": EXE_ASSET_NAME, "browser_download_url": fixture.exe_url, "size": fixture.exe_bytes.len()},
                    {"name": CHECKSUM_ASSET_NAME, "browser_download_url": fixture.checksum_url}
                ]
            }))
        }
        async fn exe(AxumState(fixture): AxumState<Fixture>) -> Vec<u8> {
            fixture.exe_bytes.to_vec()
        }
        async fn checksum(AxumState(fixture): AxumState<Fixture>) -> String {
            fixture.checksum_body.clone()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let fixture = Fixture {
            tag: tag.to_owned(),
            exe_bytes,
            checksum_body,
            exe_url: format!("{base}/exe"),
            checksum_url: format!("{base}/sha"),
        };
        let router = Router::new()
            .route("/latest", get(latest))
            .route("/exe", get(exe))
            .route("/sha", get(checksum))
            .with_state(fixture);
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        base
    }

    fn hash_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    #[tokio::test]
    async fn check_latest_reports_update_available_for_a_newer_tag() {
        static EXE: &[u8] = b"fake exe bytes";
        let base = serve_release("v99.0.0", EXE, format!("{}\n", hash_hex(EXE))).await;
        let http = reqwest::Client::new();
        let check = check_latest(&http, &format!("{base}/latest"))
            .await
            .unwrap();
        assert_eq!(check.release.version, "99.0.0");
        assert!(check.update_available);
    }

    #[tokio::test]
    async fn check_latest_reports_no_update_for_current_version() {
        static EXE: &[u8] = b"fake exe bytes";
        let current = current_version();
        let base = serve_release(current, EXE, format!("{}\n", hash_hex(EXE))).await;
        let http = reqwest::Client::new();
        let check = check_latest(&http, &format!("{base}/latest"))
            .await
            .unwrap();
        assert!(!check.update_available);
    }

    #[tokio::test]
    async fn download_and_verify_succeeds_on_matching_hash() {
        static EXE: &[u8] = b"the real deal executable payload";
        let base = serve_release(
            "v9.9.9",
            EXE,
            format!("{}  stt-server-next.exe\n", hash_hex(EXE)),
        )
        .await;
        let release = ReleaseInfo {
            version: "9.9.9".to_owned(),
            exe: ReleaseAsset {
                name: EXE_ASSET_NAME.to_owned(),
                url: format!("{base}/exe"),
                size: Some(EXE.len() as u64),
            },
            checksum: ReleaseAsset {
                name: CHECKSUM_ASSET_NAME.to_owned(),
                url: format!("{base}/sha"),
                size: None,
            },
        };
        let stage = temp_dir();
        let http = reqwest::Client::new();
        let staged = download_and_verify(&http, &release, &stage).await.unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), EXE);
        std::fs::remove_dir_all(&stage).unwrap();
    }

    #[tokio::test]
    async fn download_and_verify_fails_and_cleans_up_on_hash_mismatch() {
        static EXE: &[u8] = b"tampered-looking payload";
        let base = serve_release("v9.9.9", EXE, "0".repeat(64)).await;
        let release = ReleaseInfo {
            version: "9.9.9".to_owned(),
            exe: ReleaseAsset {
                name: EXE_ASSET_NAME.to_owned(),
                url: format!("{base}/exe"),
                size: Some(EXE.len() as u64),
            },
            checksum: ReleaseAsset {
                name: CHECKSUM_ASSET_NAME.to_owned(),
                url: format!("{base}/sha"),
                size: None,
            },
        };
        let stage = temp_dir();
        let http = reqwest::Client::new();
        let error = download_and_verify(&http, &release, &stage)
            .await
            .unwrap_err();
        assert!(matches!(error, UpdateError::Verify(_)));
        let entries: Vec<_> = std::fs::read_dir(&stage)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            entries.is_empty(),
            "staged (unverified) executable must not be left behind: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&stage);
    }
}
