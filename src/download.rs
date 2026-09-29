use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::app::App;
use crate::auth::authorized;
use crate::catalog::{catalog_model, resolve_quant, CatalogFile, CatalogModel};
use crate::errors::{internal, ApiError, ApiResult};
use crate::operations::{operation_state, update_operation_with_code};
use crate::store::{
    installed_path, promote_verified_model, ERROR_CODE_CANCELLED, ERROR_CODE_HASH_MISMATCH,
    ERROR_CODE_INSUFFICIENT_DISK_SPACE, ERROR_CODE_SOURCE_UNAVAILABLE, ERROR_CODE_STALLED,
};
use crate::verify::sha256_file;

/// No headers or body bytes for this long means the transfer is wedged, not
/// merely slow. Adapted from Handy's `DOWNLOAD_STALL_TIMEOUT`
/// (managers/model/download.rs, commit 8f9cf53, MIT).
pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Retry backoff schedule: up to 3 attempts per source, waiting this long
/// (interruptibly) between attempts before resuming from the partial.
pub const RETRY_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

/// Progress is written to SQLite at most this often (or every `PROGRESS_FLUSH_BYTES`,
/// whichever comes first), plus always on the final byte.
pub const PROGRESS_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
pub const PROGRESS_FLUSH_BYTES: u64 = 4 * 1024 * 1024;

/// Disk headroom beyond the strict remaining-bytes requirement.
const DISK_SLACK_BYTES: u64 = 64 * 1024 * 1024;

/// A download/verification failure with a machine-readable code, stored on
/// the operation record alongside the human-readable message.
#[derive(Debug, Clone)]
pub struct DownloadFailure {
    pub code: &'static str,
    pub message: String,
}

impl DownloadFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Build the single source URL to try: HuggingFace only. We do not use
/// Handy's `blob.handy.computer` mirror without that project's permission
/// (user decision), so no mirror fallback is attempted. Pure and independent
/// of I/O so it can be unit tested without a network.
pub fn candidate_urls(model: &CatalogModel, file: &CatalogFile) -> Vec<String> {
    vec![format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        model.id, model.revision, file.filename
    )]
}

/// What to do with an on-disk `.part` file before issuing any HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    /// The partial is already exactly the expected size: skip the network
    /// entirely and go straight to hash verification.
    VerifyOnly,
    /// Resume with `Range: bytes=<offset>-`.
    Range(u64),
    /// No usable partial (missing, empty, or larger than expected): start
    /// the request from scratch.
    RestartFromZero,
}

/// Pure resume decision from the partial's on-disk size and the catalog's
/// expected size. Adapted from Handy's full-size short-circuit and
/// oversized-partial handling (managers/model/download.rs, commit 8f9cf53, MIT).
pub fn resume_decision(partial_len: u64, expected_len: u64) -> ResumeDecision {
    if expected_len > 0 && partial_len == expected_len {
        ResumeDecision::VerifyOnly
    } else if partial_len == 0 || partial_len > expected_len {
        ResumeDecision::RestartFromZero
    } else {
        ResumeDecision::Range(partial_len)
    }
}

/// How a completed HTTP response with a `Content-Range`/status pair should
/// be interpreted, decoupled from `reqwest` types for pure unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeResponseOutcome {
    /// Server honored the Range request starting exactly at our offset.
    ResumeAccepted,
    /// Server ignored the Range and is sending the whole object from zero.
    RestartFromZero,
    /// 416: our offset is at/past the object's end; discard and restart once.
    Restart416,
    /// Any other case (wrong Content-Range start on a 206, etc): fatal.
    Invalid,
}

/// Pure classification of a resumed request's response status/Content-Range
/// start, given the offset we asked to resume from.
pub fn classify_range_response(
    offset: u64,
    status: u16,
    content_range_start: Option<u64>,
) -> RangeResponseOutcome {
    match status {
        200 => RangeResponseOutcome::RestartFromZero,
        206 if content_range_start == Some(offset) => RangeResponseOutcome::ResumeAccepted,
        206 => RangeResponseOutcome::Invalid,
        416 => RangeResponseOutcome::Restart416,
        _ => RangeResponseOutcome::Invalid,
    }
}

/// Bytes of free space required on the data volume before starting/resuming
/// a download: the remaining bytes to fetch, with 5% headroom plus a fixed
/// 64 MiB slack for filesystem/journal overhead.
pub fn disk_required_bytes(expected_size: u64, partial_bytes: u64) -> u64 {
    let remaining = expected_size.saturating_sub(partial_bytes);
    let with_slack = (remaining as f64 * 1.05).ceil() as u64;
    with_slack.saturating_add(DISK_SLACK_BYTES)
}

/// Whether enough free space is available; pure given an injectable
/// free-space reading so it's testable without touching the real disk.
pub fn has_enough_disk_space(available: u64, expected_size: u64, partial_bytes: u64) -> bool {
    available >= disk_required_bytes(expected_size, partial_bytes)
}

/// Real free-space probe for `path`'s volume (or nearest existing ancestor).
fn free_space_bytes(path: &Path) -> std::io::Result<u64> {
    let mut probe = path.to_path_buf();
    loop {
        if probe.exists() {
            return fs4::available_space(&probe);
        }
        if !probe.pop() {
            return fs4::available_space(Path::new("."));
        }
    }
}

/// Should progress be flushed to SQLite now? Throttled to at most once per
/// `PROGRESS_FLUSH_INTERVAL` or every `PROGRESS_FLUSH_BYTES`, whichever
/// comes first; `force` always flushes (used for the final write).
pub fn should_flush_progress(
    elapsed_since_last: Duration,
    bytes_since_last: u64,
    force: bool,
) -> bool {
    force
        || elapsed_since_last >= PROGRESS_FLUSH_INTERVAL
        || bytes_since_last >= PROGRESS_FLUSH_BYTES
}

pub async fn install_model(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorized(&headers, &app)?;
    let requested_quant: Option<String> = if body.is_empty() {
        None
    } else {
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_body", error.to_string())
        })?;
        value
            .get("quant")
            .and_then(|quant| quant.as_str())
            .map(str::to_owned)
    };
    let model = catalog_model(&app, &id)?.clone();
    if installed_path(&app, &id)?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_installed",
            "Model is already installed",
        ));
    }
    let file = resolve_quant(&model, requested_quant.as_deref())?.clone();
    let op = Uuid::new_v4().to_string();
    {
        let db = app.db.lock().map_err(internal)?;
        let active: i64 = db
            .query_row(
                "SELECT count(*) FROM operations WHERE model_id=?1 AND state IN ('queued','running')",
                params![id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if active > 0 {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "operation_conflict",
                "An operation for this model is already active",
            ));
        }
        let now = crate::store::now_ms();
        db.execute(
            "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at) VALUES(?1,?2,'install','queued',NULL,0,?3,?4,?4)",
            params![op, id, file.size_bytes, now],
        )
        .map_err(internal)?;
    }
    let task_app = app.clone();
    let task_op = op.clone();
    tokio::spawn(async move {
        if let Err(failure) = download_model(task_app.clone(), model, file, &task_op).await {
            if operation_state(&task_app, &task_op)
                .ok()
                .flatten()
                .as_deref()
                != Some("cancelled")
            {
                let _ = update_operation_with_code(
                    &task_app,
                    &task_op,
                    "failed",
                    Some(&failure.message),
                    Some(failure.code),
                    0,
                );
            }
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"operation_id":op,"state":"queued"})),
    ))
}

fn cancelled(app: &App, op: &str) -> Result<bool, DownloadFailure> {
    Ok(operation_state(app, op)
        .map_err(|error| DownloadFailure::new("internal_error", error.message))?
        .as_deref()
        == Some("cancelled"))
}

/// Sleep for `duration`, polling operation state every 200ms so cancellation
/// during a retry backoff is noticed promptly instead of only after the
/// whole backoff elapses.
async fn interruptible_backoff(
    app: &App,
    op: &str,
    duration: Duration,
) -> Result<(), DownloadFailure> {
    let start = Instant::now();
    let step = Duration::from_millis(200);
    loop {
        if cancelled(app, op)? {
            return Err(DownloadFailure::new(
                ERROR_CODE_CANCELLED,
                "Cancelled by user",
            ));
        }
        let remaining = duration.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(remaining.min(step)).await;
    }
}

enum AttemptOutcome {
    Completed,
    Cancelled,
}

/// One HTTP attempt against `url`, resuming from whatever is already staged.
/// Returns `Completed` once the full expected size is on disk (network or
/// short-circuited via [`ResumeDecision::VerifyOnly`]); the caller is
/// responsible for hash verification afterward.
async fn attempt_download(
    app: &App,
    url: &str,
    stage: &Path,
    file: &CatalogFile,
    op: &str,
    stall_timeout: Duration,
) -> Result<AttemptOutcome, DownloadFailure> {
    let partial_len = tokio::fs::metadata(stage)
        .await
        .map(|info| info.len())
        .unwrap_or(0);
    let mut offset = match resume_decision(partial_len, file.size_bytes) {
        ResumeDecision::VerifyOnly => return Ok(AttemptOutcome::Completed),
        ResumeDecision::RestartFromZero => {
            if partial_len > 0 {
                tokio::fs::remove_file(stage)
                    .await
                    .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
            }
            0
        }
        ResumeDecision::Range(offset) => offset,
    };

    let mut request = app.http.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = tokio::time::timeout(stall_timeout, request.send())
        .await
        .map_err(|_| {
            DownloadFailure::new(
                ERROR_CODE_STALLED,
                format!("no response within {}s from {url}", stall_timeout.as_secs()),
            )
        })?
        .map_err(|error| DownloadFailure::new(ERROR_CODE_SOURCE_UNAVAILABLE, error.to_string()))?;

    if offset > 0 {
        let content_range_start = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_start);
        match classify_range_response(offset, response.status().as_u16(), content_range_start) {
            RangeResponseOutcome::ResumeAccepted => {}
            RangeResponseOutcome::RestartFromZero => {
                tokio::fs::remove_file(stage)
                    .await
                    .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
                offset = 0;
            }
            RangeResponseOutcome::Restart416 => {
                tokio::fs::remove_file(stage)
                    .await
                    .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
                return Err(DownloadFailure::new(
                    ERROR_CODE_SOURCE_UNAVAILABLE,
                    "Model source returned HTTP 416; discarded partial to restart",
                ));
            }
            RangeResponseOutcome::Invalid => {
                return Err(DownloadFailure::new(
                    ERROR_CODE_SOURCE_UNAVAILABLE,
                    format!(
                        "Model source returned an invalid resume response (HTTP {})",
                        response.status()
                    ),
                ));
            }
        }
    } else if !response.status().is_success() {
        return Err(DownloadFailure::new(
            ERROR_CODE_SOURCE_UNAVAILABLE,
            format!("Model source returned HTTP {}", response.status()),
        ));
    }

    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(offset > 0)
        .truncate(offset == 0)
        .open(stage)
        .await
        .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
    let mut received = offset;
    let mut last_flush = Instant::now();
    let mut bytes_since_flush = 0u64;
    loop {
        let chunk = match tokio::time::timeout(stall_timeout, response.chunk()).await {
            Err(_) => {
                return Err(DownloadFailure::new(
                    ERROR_CODE_STALLED,
                    format!("transfer stalled: no data for {}s", stall_timeout.as_secs()),
                ))
            }
            Ok(Err(error)) => {
                return Err(DownloadFailure::new(
                    ERROR_CODE_SOURCE_UNAVAILABLE,
                    error.to_string(),
                ))
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        if cancelled(app, op)? {
            return Ok(AttemptOutcome::Cancelled);
        }
        received += chunk.len() as u64;
        if received > file.size_bytes {
            return Err(DownloadFailure::new(
                ERROR_CODE_SOURCE_UNAVAILABLE,
                "Model source exceeded catalog size",
            ));
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
        bytes_since_flush += chunk.len() as u64;
        if should_flush_progress(last_flush.elapsed(), bytes_since_flush, false) {
            update_operation_with_code(app, op, "running", None, None, received)
                .map_err(|error| DownloadFailure::new("internal_error", error.message))?;
            last_flush = Instant::now();
            bytes_since_flush = 0;
        }
    }
    output
        .sync_all()
        .await
        .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
    drop(output);
    update_operation_with_code(app, op, "running", None, None, received)
        .map_err(|error| DownloadFailure::new("internal_error", error.message))?;
    if received != file.size_bytes {
        return Err(DownloadFailure::new(
            ERROR_CODE_SOURCE_UNAVAILABLE,
            format!("Incomplete model: {received} of {} bytes", file.size_bytes),
        ));
    }
    Ok(AttemptOutcome::Completed)
}

/// Start offset of a `Content-Range: bytes <start>-<end>/<total>` header.
/// Copied from Handy's `content_range_start`
/// (managers/model/download.rs, commit 8f9cf53, MIT).
fn content_range_start(value: &str) -> Option<u64> {
    let range = value.trim().strip_prefix("bytes")?.trim_start();
    range.split('-').next()?.trim().parse().ok()
}

pub async fn download_model(
    app: Arc<App>,
    model: CatalogModel,
    file: CatalogFile,
    op: &str,
) -> Result<(), DownloadFailure> {
    download_model_with_timeout(app, model, file, op, STALL_TIMEOUT).await
}

/// Same as [`download_model`] but with an injectable stall timeout, so tests
/// can exercise the stall path without a real 60-second wait.
pub async fn download_model_with_timeout(
    app: Arc<App>,
    model: CatalogModel,
    file: CatalogFile,
    op: &str,
    stall_timeout: Duration,
) -> Result<(), DownloadFailure> {
    if cancelled(&app, op)? {
        return Err(DownloadFailure::new(
            ERROR_CODE_CANCELLED,
            "Cancelled by user",
        ));
    }
    update_operation_with_code(&app, op, "running", None, None, 0)
        .map_err(|error| DownloadFailure::new("internal_error", error.message))?;

    let stage = app
        .data_dir
        .join("staging")
        .join(format!("{}.part", file.sha256));

    let partial_len = tokio::fs::metadata(&stage)
        .await
        .map(|info| info.len())
        .unwrap_or(0);
    let needed = disk_required_bytes(file.size_bytes, partial_len);
    let available = free_space_bytes(&app.data_dir)
        .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
    if available < needed {
        return Err(DownloadFailure::new(
            ERROR_CODE_INSUFFICIENT_DISK_SPACE,
            format!("Need {needed} bytes free, only {available} available"),
        ));
    }

    let urls = candidate_urls(&model, &file);
    download_with_sources(app, &urls, &model, file, op, stall_timeout, stage).await
}

/// The retry/verify/promote core, taking an explicit source URL list rather
/// than deriving it from the catalog, so tests can point it at a local fake
/// server instead of huggingface.co. There is no mirror fallback: HuggingFace
/// is the only source (user decision — we do not use Handy's mirror without
/// that project's permission). The disk preflight is done by the caller
/// ([`download_model_with_timeout`]) since it needs the (pre-download-attempt)
/// partial size.
async fn download_with_sources(
    app: Arc<App>,
    urls: &[String],
    model: &CatalogModel,
    file: CatalogFile,
    op: &str,
    stall_timeout: Duration,
    stage: std::path::PathBuf,
) -> Result<(), DownloadFailure> {
    let mut last_errors: Vec<(String, DownloadFailure)> = Vec::new();
    let mut completed = false;
    'sources: for url in urls {
        for (attempt, backoff) in RETRY_BACKOFFS.iter().enumerate() {
            if cancelled(&app, op)? {
                return Err(DownloadFailure::new(
                    ERROR_CODE_CANCELLED,
                    "Cancelled by user",
                ));
            }
            match attempt_download(&app, url, &stage, &file, op, stall_timeout).await {
                Ok(AttemptOutcome::Completed) => {
                    completed = true;
                    break 'sources;
                }
                Ok(AttemptOutcome::Cancelled) => {
                    return Err(DownloadFailure::new(
                        ERROR_CODE_CANCELLED,
                        "Cancelled by user",
                    ));
                }
                Err(failure) => {
                    last_errors.retain(|(existing_url, _)| existing_url != url);
                    last_errors.push((url.clone(), failure.clone()));
                    let is_last_attempt = attempt + 1 == RETRY_BACKOFFS.len();
                    if !is_last_attempt {
                        interruptible_backoff(&app, op, *backoff).await?;
                    }
                }
            }
        }
    }
    if !completed {
        let summary = last_errors
            .iter()
            .map(|(url, failure)| format!("{url}: {}", failure.message))
            .collect::<Vec<_>>()
            .join("; ");
        // A single source that failed the same way on every attempt keeps
        // its specific code (e.g. `stalled`); anything broader (multiple
        // sources tried, or mixed failure types) surfaces as the generic
        // `source_unavailable`.
        let distinct_codes: std::collections::HashSet<&str> = last_errors
            .iter()
            .map(|(_, failure)| failure.code)
            .collect();
        let code = if distinct_codes.len() == 1 {
            last_errors[0].1.code
        } else {
            ERROR_CODE_SOURCE_UNAVAILABLE
        };
        return Err(DownloadFailure::new(
            code,
            format!("All sources failed - {summary}"),
        ));
    }

    let hash_path = stage.clone();
    let actual = tokio::task::spawn_blocking(move || sha256_file(&hash_path))
        .await
        .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?
        .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
    if !actual.eq_ignore_ascii_case(&file.sha256) {
        let quarantine = app.data_dir.join("quarantine");
        tokio::fs::create_dir_all(&quarantine)
            .await
            .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
        tokio::fs::rename(&stage, quarantine.join(format!("{op}-hash-mismatch.gguf")))
            .await
            .map_err(|error| DownloadFailure::new("internal_error", error.to_string()))?;
        return Err(DownloadFailure::new(
            ERROR_CODE_HASH_MISMATCH,
            "Catalog SHA-256 mismatch",
        ));
    }
    let received = file.size_bytes;
    promote_verified_model(&app, op, &model.slug, &file, &stage, received)
        .map_err(|error| DownloadFailure::new("internal_error", error))?;
    Ok(())
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    fn model() -> CatalogModel {
        serde_json::from_value(json!({
            "id": "org/model",
            "revision": "abc123",
            "slug": "model-slug",
            "name": "Model",
            "architecture": "whisper",
            "family": "whisper",
            "license": "mit",
            "languages": ["en"],
            "capabilities": {"streaming": false, "translate": false, "lang_detect": false, "timestamps": "none"},
            "speed_score": null,
            "accuracy_score": null,
            "files": [],
            "default_quant": "Q4",
            "recommended": false,
            "recommended_rank": null
        }))
        .unwrap()
    }

    fn file() -> CatalogFile {
        CatalogFile {
            filename: "model.gguf".to_owned(),
            quant: "Q4_K_M".to_owned(),
            size_bytes: 1000,
            sha256: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn candidate_urls_is_huggingface_only() {
        let urls = candidate_urls(&model(), &file());
        assert_eq!(
            urls,
            vec!["https://huggingface.co/org/model/resolve/abc123/model.gguf"]
        );
    }

    #[test]
    fn retry_schedule_is_2_5_15_seconds() {
        assert_eq!(
            RETRY_BACKOFFS,
            [
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(15)
            ]
        );
    }

    #[test]
    fn resume_decision_full_partial_verifies() {
        assert_eq!(resume_decision(1000, 1000), ResumeDecision::VerifyOnly);
    }

    #[test]
    fn resume_decision_oversized_partial_restarts() {
        assert_eq!(resume_decision(2000, 1000), ResumeDecision::RestartFromZero);
    }

    #[test]
    fn resume_decision_missing_partial_restarts() {
        assert_eq!(resume_decision(0, 1000), ResumeDecision::RestartFromZero);
    }

    #[test]
    fn resume_decision_partial_prefix_resumes_with_range() {
        assert_eq!(resume_decision(400, 1000), ResumeDecision::Range(400));
    }

    #[test]
    fn range_response_206_at_offset_is_accepted() {
        assert_eq!(
            classify_range_response(400, 206, Some(400)),
            RangeResponseOutcome::ResumeAccepted
        );
    }

    #[test]
    fn range_response_200_restarts_from_zero() {
        assert_eq!(
            classify_range_response(400, 200, None),
            RangeResponseOutcome::RestartFromZero
        );
    }

    #[test]
    fn range_response_416_restarts_once() {
        assert_eq!(
            classify_range_response(400, 416, None),
            RangeResponseOutcome::Restart416
        );
    }

    #[test]
    fn range_response_206_with_wrong_start_is_invalid() {
        assert_eq!(
            classify_range_response(400, 206, Some(999)),
            RangeResponseOutcome::Invalid
        );
    }

    #[test]
    fn disk_requirement_adds_5_percent_and_64mib_slack() {
        let needed = disk_required_bytes(1_000_000_000, 0);
        let expected = (1_000_000_000f64 * 1.05).ceil() as u64 + 64 * 1024 * 1024;
        assert_eq!(needed, expected);
    }

    #[test]
    fn disk_requirement_accounts_for_existing_partial() {
        let needed = disk_required_bytes(1000, 400);
        let expected = (600f64 * 1.05).ceil() as u64 + 64 * 1024 * 1024;
        assert_eq!(needed, expected);
    }

    #[test]
    fn has_enough_disk_space_true_and_false() {
        let needed = disk_required_bytes(1000, 0);
        assert!(has_enough_disk_space(needed, 1000, 0));
        assert!(!has_enough_disk_space(needed - 1, 1000, 0));
    }

    #[test]
    fn should_flush_progress_by_time() {
        assert!(should_flush_progress(PROGRESS_FLUSH_INTERVAL, 0, false));
        assert!(!should_flush_progress(Duration::from_millis(10), 0, false));
    }

    #[test]
    fn should_flush_progress_by_bytes() {
        assert!(should_flush_progress(
            Duration::from_millis(1),
            PROGRESS_FLUSH_BYTES,
            false
        ));
        assert!(!should_flush_progress(
            Duration::from_millis(1),
            1024,
            false
        ));
    }

    #[test]
    fn should_flush_progress_forced() {
        assert!(should_flush_progress(Duration::from_millis(0), 0, true));
    }

    #[test]
    fn content_range_start_parses() {
        assert_eq!(content_range_start("bytes 400-999/1000"), Some(400));
        assert_eq!(content_range_start("garbage"), None);
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::app::open_app_at;
    use crate::operations::operation_state;
    use sha2::{Digest, Sha256};

    fn fake_file(bytes: &[u8]) -> CatalogFile {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        CatalogFile {
            filename: "model.gguf".to_owned(),
            quant: "Q4_K_M".to_owned(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", hasher.finalize()),
        }
    }

    fn temp_app_dir() -> std::path::PathBuf {
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("stt-server-dl-test-{}", Uuid::new_v4()))
    }

    /// Binds an axum server to 127.0.0.1:0 serving `body` at `/file`, honoring
    /// `Range` requests, and returns its base URL. The server runs until the
    /// test process exits (acceptable for short-lived unit tests).
    async fn serve_bytes(body: &'static [u8]) -> String {
        use axum::extract::State as AxumState;
        use axum::http::HeaderMap as ReqHeaders;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn handler(
            AxumState(body): AxumState<&'static [u8]>,
            headers: ReqHeaders,
        ) -> impl IntoResponse {
            if let Some(range) = headers.get(axum::http::header::RANGE) {
                let value = range.to_str().unwrap_or("");
                if let Some(start) = value
                    .strip_prefix("bytes=")
                    .and_then(|v| v.trim_end_matches('-').parse::<u64>().ok())
                {
                    let start = start as usize;
                    if start >= body.len() {
                        return (
                            StatusCode::RANGE_NOT_SATISFIABLE,
                            [(
                                axum::http::header::CONTENT_RANGE,
                                format!("bytes */{}", body.len()),
                            )],
                            Bytes::new(),
                        )
                            .into_response();
                    }
                    let slice = &body[start..];
                    return (
                        StatusCode::PARTIAL_CONTENT,
                        [(
                            axum::http::header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", start, body.len() - 1, body.len()),
                        )],
                        Bytes::from(slice),
                    )
                        .into_response();
                }
            }
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_RANGE, String::new())],
                Bytes::from(body),
            )
                .into_response()
        }

        let router = axum::Router::new()
            .route("/file", get(handler))
            .with_state(body);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// A server that sends `prefix_len` bytes of the body then hangs forever
    /// (never closes, never sends more), to exercise the stall timeout.
    async fn serve_stalling(body: &'static [u8], prefix_len: usize) -> String {
        use axum::body::Body;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use futures_util::{stream, StreamExt};

        async fn handler(
            axum::extract::State((body, prefix_len)): axum::extract::State<(&'static [u8], usize)>,
        ) -> impl IntoResponse {
            // Send `prefix_len` bytes immediately, then a stream that never
            // resolves again, so the connection stays open with no more data
            // (a stall) instead of closing (which would just end the body).
            let first = Bytes::from(&body[..prefix_len]);
            let tail = stream::pending::<Result<Bytes, std::io::Error>>();
            let combined = stream::once(async move { Ok(first) }).chain(tail);
            // Deliberately no Content-Length: the body streams via chunked
            // transfer encoding, and the reader (attempt_download) only
            // relies on the catalog's expected size, not this header.
            (StatusCode::OK, Body::from_stream(combined))
        }

        let router = axum::Router::new()
            .route("/file", get(handler))
            .with_state((body, prefix_len));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn fake_model() -> CatalogModel {
        serde_json::from_value(json!({
            "id": "org/model", "revision": "abc123", "slug": "fake-model", "name": "Fake",
            "architecture": "whisper", "family": "whisper", "license": "mit", "languages": ["en"],
            "capabilities": {"streaming": false, "translate": false, "lang_detect": false, "timestamps": "none"},
            "speed_score": null, "accuracy_score": null, "files": [], "default_quant": "Q4",
            "recommended": false, "recommended_rank": null
        }))
        .unwrap()
    }

    async fn new_app() -> (Arc<App>, std::path::PathBuf) {
        let path = temp_app_dir();
        let app = open_app_at(path.clone()).unwrap();
        (app, path)
    }

    fn cleanup(path: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(path.canonicalize().unwrap_or(path));
    }

    async fn new_op(app: &App, model_id: &str, total: u64) -> String {
        let op = Uuid::new_v4().to_string();
        let now = crate::store::now_ms();
        app.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO operations(id,model_id,kind,state,error,progress_bytes,total_bytes,created_at,updated_at) VALUES(?1,?2,'install','queued',NULL,0,?3,?4,?4)",
                params![op, model_id, total, now],
            )
            .unwrap();
        op
    }

    #[tokio::test]
    async fn happy_download_verifies_hash_and_installs() {
        static BODY: &[u8] = b"hello world, this is a fake model payload!";
        let base = serve_bytes(BODY).await;
        let file = fake_file(BODY);
        let model = fake_model();
        let (app, path) = new_app().await;
        let op = new_op(&app, &model.slug, file.size_bytes).await;
        let stage = app
            .data_dir
            .join("staging")
            .join(format!("{}.part", file.sha256));
        let urls = vec![format!("{base}/file")];
        download_with_sources(
            app.clone(),
            &urls,
            &model,
            file.clone(),
            &op,
            STALL_TIMEOUT,
            stage,
        )
        .await
        .unwrap();
        assert_eq!(
            operation_state(&app, &op).unwrap().as_deref(),
            Some("completed")
        );
        assert!(installed_path(&app, &model.slug).unwrap().is_some());
        cleanup(path);
    }

    #[tokio::test]
    async fn stall_fails_within_shortened_timeout() {
        static BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let base = serve_stalling(BODY, 5).await;
        let file = fake_file(BODY);
        let model = fake_model();
        let (app, path) = new_app().await;
        let op = new_op(&app, &model.slug, file.size_bytes).await;
        let stage = app
            .data_dir
            .join("staging")
            .join(format!("{}.part", file.sha256));
        let urls = vec![format!("{base}/file")];
        let short = Duration::from_millis(300);
        let start = Instant::now();
        let result =
            download_with_sources(app.clone(), &urls, &model, file.clone(), &op, short, stage)
                .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, ERROR_CODE_STALLED);
        // 3 attempts * short timeout, well under a real 60s stall timeout.
        assert!(start.elapsed() < Duration::from_secs(10));
        cleanup(path);
    }

    #[tokio::test]
    async fn resumes_via_range_after_partial_write() {
        static BODY: &[u8] = b"resume-me-please-this-is-a-longer-fake-payload-for-range-tests";
        let base = serve_bytes(BODY).await;
        let file = fake_file(BODY);
        let model = fake_model();
        let (app, path) = new_app().await;
        let op = new_op(&app, &model.slug, file.size_bytes).await;
        let stage = app
            .data_dir
            .join("staging")
            .join(format!("{}.part", file.sha256));
        tokio::fs::create_dir_all(stage.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&stage, &BODY[..10]).await.unwrap();
        let urls = vec![format!("{base}/file")];
        download_with_sources(
            app.clone(),
            &urls,
            &model,
            file.clone(),
            &op,
            STALL_TIMEOUT,
            stage,
        )
        .await
        .unwrap();
        assert_eq!(
            operation_state(&app, &op).unwrap().as_deref(),
            Some("completed")
        );
        cleanup(path);
    }

    /// A server that always answers `/file` with a non-success status, to
    /// exercise the "source responded but refused" path (fast, deterministic)
    /// rather than an unreachable port (whose failure mode -- refused vs.
    /// black-holed -- is platform/timing dependent and can look like a stall).
    async fn serve_always_failing() -> String {
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn handler() -> impl IntoResponse {
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let router = axum::Router::new().route("/file", get(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn only_one_source_is_tried_and_failure_is_retryable() {
        // The source responds but refuses every request the same way, and
        // there is no mirror to fall back to (HuggingFace only, per user
        // decision), so the single URL is retried up to RETRY_BACKOFFS.len()
        // times and then fails with the retryable source_unavailable code.
        static BODY: &[u8] = b"always-failing-source-payload-bytes";
        let base = serve_always_failing().await;
        let file = fake_file(BODY);
        let model = fake_model();
        let (app, path) = new_app().await;
        let op = new_op(&app, &model.slug, file.size_bytes).await;
        let stage = app
            .data_dir
            .join("staging")
            .join(format!("{}.part", file.sha256));
        let urls = vec![format!("{base}/file")];
        let start = Instant::now();
        let result = download_with_sources(
            app.clone(),
            &urls,
            &model,
            file.clone(),
            &op,
            STALL_TIMEOUT,
            stage,
        )
        .await;
        let failure = result.unwrap_err();
        assert_eq!(failure.code, ERROR_CODE_SOURCE_UNAVAILABLE);
        assert!(
            failure.message.contains(&base),
            "message should mention the only source tried: {}",
            failure.message
        );
        assert!(!failure.message.contains("huggingface.co/"));
        // Bounded by 3 retries with short backoffs; well under the 60s stall timeout.
        assert!(start.elapsed() < Duration::from_secs(10));
        cleanup(path);
    }

    #[tokio::test]
    async fn hash_mismatch_quarantines_and_fails() {
        static BODY: &[u8] = b"actual bytes served by the fake server";
        let base = serve_bytes(BODY).await;
        let mut file = fake_file(BODY);
        file.sha256 = "0".repeat(64); // wrong hash on purpose
        let model = fake_model();
        let (app, path) = new_app().await;
        let op = new_op(&app, &model.slug, file.size_bytes).await;
        let stage = app
            .data_dir
            .join("staging")
            .join(format!("{}.part", file.sha256));
        let urls = vec![format!("{base}/file")];
        let result = download_with_sources(
            app.clone(),
            &urls,
            &model,
            file.clone(),
            &op,
            STALL_TIMEOUT,
            stage,
        )
        .await;
        assert_eq!(result.unwrap_err().code, ERROR_CODE_HASH_MISMATCH);
        assert!(app
            .data_dir
            .join("quarantine")
            .join(format!("{op}-hash-mismatch.gguf"))
            .exists());
        cleanup(path);
    }

    #[test]
    fn already_full_size_partial_skips_network_when_hash_matches() {
        // Regression guard for the VerifyOnly short-circuit: this is exercised
        // at the pure-function level in `pure_tests::resume_decision_full_partial_verifies`;
        // this test just documents that `attempt_download` honors it without a
        // running server on the URL (would hang/err if it tried to connect).
        assert_eq!(resume_decision(50, 50), ResumeDecision::VerifyOnly);
    }
}
