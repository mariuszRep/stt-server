//! Live transcription over WebSocket: `GET /v1/realtime?intent=transcription`.
//!
//! The wire format follows OpenAI's Realtime API in transcription mode
//! (client-driven turns: `turn_detection` is always null). On top of OpenAI's
//! `...transcription.delta` / `.completed` events it adds one extension event,
//! `conversation.item.input_audio_transcription.partial`, that carries the
//! engine's committed/tentative split (the semantics Handy uses for live
//! preview). `docs/client-contract.md` section 5a is the public reference,
//! including every deviation from OpenAI.
//!
//! Concurrency (see also `crate::queue`): each connection owns one async task
//! (this module) and, while an audio item is in flight, one blocking worker
//! thread with its own `transcribe_cpp::Session`. Two `Session`s of one loaded
//! `Model` are safe (`Model` is `Send + Sync`), but the library holds a
//! per-model compute lease for the whole life of a stream and refuses any
//! overlapping run on that model with `Busy`. So a stream cannot share its
//! model with a concurrent batch run, and the design is:
//!
//! - at most one live stream server-wide (the queue's stream slot; a second
//!   gets `stream_busy` immediately);
//! - the stream takes the single batch permit only while it begins (model
//!   bind/swap and `Session::stream`), in FIFO order with batch requests, and
//!   releases it right after. A batch request for a different model is never
//!   queued behind a running stream; one for the same model gets
//!   `503 engine_busy` until the stream finishes (items are short dictation
//!   utterances).

use std::sync::mpsc;
use std::time::Duration;

use axum::extract::ws::{
    rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use transcribe_cpp::{Model, RunOptions, StreamOptions};
use uuid::Uuid;

use crate::api::{
    bind_or_swap_model, queue_api_error, require_installed_verified, resolve_requested_model,
};
use crate::app::App;
use crate::audio::PcmStream;
use crate::auth::{authorize, AccessLevel};
use crate::capabilities::EffectiveCaps;
use crate::errors::{internal, map_engine_error, ApiError};
use crate::run_plan::{plan, Endpoint, ParsedRequest};

/// An item that receives no client event for this long is discarded.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// OpenAI's `pcm16` is 24 kHz mono; that is the default here too.
const DEFAULT_SAMPLE_RATE: u32 = 24_000;

#[derive(Deserialize)]
pub struct ConnectQuery {
    intent: Option<String>,
    model: Option<String>,
}

pub async fn realtime(
    State(app): State<std::sync::Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<ConnectQuery>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    authorize(&headers, &app, AccessLevel::User)?;
    if query
        .intent
        .as_deref()
        .is_some_and(|i| i != "transcription")
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_intent",
            "Only intent=transcription is supported",
        ));
    }
    let upgrade = upgrade.map_err(|rejection| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "websocket_required",
            rejection.to_string(),
        )
    })?;
    Ok(upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let mut connection = Connection::new(app, socket, query.model);
            let _ = connection.run().await;
        })
        .into_response())
}

// ---------------------------------------------------------------------------
// Session configuration (transcription_session.update)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct SessionConfig {
    id: String,
    model: String,
    language: Option<String>,
    /// Translate each turn into this language (extension; see section 5a of
    /// the client contract). `None`: plain transcription.
    target_language: Option<String>,
    prompt: Option<String>,
    sample_rate: u32,
}

fn invalid(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, message)
}

impl SessionConfig {
    fn new(model: Option<String>) -> Self {
        Self {
            id: format!("sess_{}", Uuid::new_v4().simple()),
            model: model.unwrap_or_else(|| "default".to_owned()),
            language: None,
            target_language: None,
            prompt: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
        }
    }

    /// Apply a `session` object. Both OpenAI shapes are read: the beta
    /// transcription session (`input_audio_format`, `input_audio_transcription`,
    /// `turn_detection`) and the GA one (`audio.input.format|transcription|
    /// turn_detection`). Validates everything before changing anything.
    fn update(&mut self, session: &Value) -> Result<(), ApiError> {
        let mut next = self.clone();
        let first = |paths: &[&str]| paths.iter().find_map(|p| session.pointer(p));

        if let Some(turn) = first(&["/turn_detection", "/audio/input/turn_detection"]) {
            if !turn.is_null() {
                return Err(invalid(
                    "turn_detection_unsupported",
                    "turn_detection must be null: the client commits each turn",
                ));
            }
        }
        if let Some(format) = session.get("input_audio_format") {
            if format != "pcm16" {
                return Err(invalid(
                    "unsupported_audio_format",
                    "input_audio_format must be \"pcm16\"",
                ));
            }
        }
        if let Some(format) = session.pointer("/audio/input/format") {
            if format.get("type").is_some_and(|t| t != "audio/pcm") {
                return Err(invalid(
                    "unsupported_audio_format",
                    "audio.input.format.type must be \"audio/pcm\"",
                ));
            }
        }
        if let Some(rate) = first(&["/input_audio_sample_rate", "/audio/input/format/rate"]) {
            next.sample_rate = rate
                .as_u64()
                .filter(|r| (8_000..=48_000).contains(r))
                .ok_or_else(|| {
                    invalid(
                        "unsupported_audio_format",
                        "the audio sample rate must be between 8000 and 48000",
                    )
                })? as u32;
        }
        if let Some(t) = first(&["/input_audio_transcription", "/audio/input/transcription"]) {
            if let Some(model) = t.get("model").and_then(Value::as_str) {
                next.model = if model.is_empty() { "default" } else { model }.to_owned();
            }
            let language = t
                .get("language")
                .and_then(Value::as_str)
                .or_else(|| t.pointer("/languages/0").and_then(Value::as_str));
            if t.get("language").is_some() || t.get("languages").is_some() {
                next.language = language.filter(|l| !l.is_empty()).map(str::to_owned);
            }
            if let Some(prompt) = t.get("prompt") {
                next.prompt = prompt.as_str().filter(|p| !p.is_empty()).map(str::to_owned);
            }
        }
        // `input_audio_transcription.target_language` is this server's name;
        // `audio.output.language` is OpenAI's name for the same thing.
        if let Some(target) = first(&[
            "/input_audio_transcription/target_language",
            "/audio/input/transcription/target_language",
            "/audio/output/language",
        ]) {
            next.target_language = target.as_str().filter(|l| !l.is_empty()).map(str::to_owned);
        }
        *self = next;
        Ok(())
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "object": "realtime.transcription_session",
            "input_audio_format": "pcm16",
            "input_audio_sample_rate": self.sample_rate,
            "input_audio_transcription": {
                "model": self.model,
                "language": self.language,
                "target_language": self.target_language,
                "prompt": self.prompt,
            },
            "turn_detection": null,
        })
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

fn event(kind: &str, mut body: Value) -> Value {
    body["type"] = json!(kind);
    body["event_id"] = json!(format!("event_{}", Uuid::new_v4().simple()));
    body
}

fn error_event(error: &ApiError) -> Value {
    let kind = if error.status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    };
    event(
        "error",
        json!({"error": {
            "type": kind,
            "code": error.code,
            "message": error.message,
            "param": null,
            "event_id": null,
        }}),
    )
}

/// The text a delta event should carry: what `committed` added to the
/// previously reported committed text. `None` when nothing was appended (the
/// committed text is append-only; anything else is not a delta).
fn delta_since<'a>(previous: &str, committed: &'a str) -> Option<&'a str> {
    committed
        .strip_prefix(previous)
        .filter(|delta| !delta.is_empty())
}

fn check_streaming(caps: &EffectiveCaps) -> Result<(), ApiError> {
    if caps.loaded.supports_streaming {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "streaming_unsupported",
            "This model does not support live streaming; use /v1/audio/transcriptions",
        ))
    }
}

// ---------------------------------------------------------------------------
// Worker (one blocking thread per in-flight item)
// ---------------------------------------------------------------------------

enum Cmd {
    Feed(Vec<f32>),
    Finalize,
}

enum Update {
    Partial {
        committed: String,
        tentative: String,
        revision: i32,
    },
    Completed(String),
    Failed(ApiError),
    /// Always last: the worker has released the stream slot.
    Ended,
}

fn run_stream(
    model: &Model,
    run: &RunOptions,
    batch_permit: tokio::sync::OwnedSemaphorePermit,
    commands: &mpsc::Receiver<Cmd>,
    updates: &UnboundedSender<Update>,
) -> Result<Option<String>, ApiError> {
    let mut session = model.session().map_err(map_engine_error)?;
    let mut stream = session
        .stream(run, &StreamOptions::default())
        .map_err(map_engine_error)?;
    // Begun: the stream now holds the model's lease; the batch queue moves on.
    drop(batch_permit);
    // A closed command channel means the client cleared or left: dropping
    // `stream` abandons it.
    while let Ok(command) = commands.recv() {
        match command {
            Cmd::Feed(pcm) => {
                let update = stream.feed(&pcm).map_err(map_engine_error)?;
                if update.committed_changed || update.tentative_changed {
                    let text = stream.text();
                    let _ = updates.send(Update::Partial {
                        committed: text.committed,
                        tentative: text.tentative,
                        revision: update.revision,
                    });
                }
            }
            Cmd::Finalize => {
                let update = stream.finalize().map_err(map_engine_error)?;
                let text = stream.text();
                // After finalize the committed prefix is the whole text.
                let _ = updates.send(Update::Partial {
                    committed: text.display(),
                    tentative: String::new(),
                    revision: update.revision,
                });
                return Ok(Some(text.full.trim().to_owned()));
            }
        }
    }
    Ok(None)
}

fn stream_worker(
    model: Model,
    run: RunOptions,
    batch_permit: tokio::sync::OwnedSemaphorePermit,
    stream_slot: tokio::sync::OwnedSemaphorePermit,
    commands: mpsc::Receiver<Cmd>,
    updates: UnboundedSender<Update>,
) {
    let outcome = run_stream(&model, &run, batch_permit, &commands, &updates);
    // Free the slot before announcing the end, so a client that reacts to
    // `Completed` by starting the next item never finds it taken.
    drop(stream_slot);
    match outcome {
        Ok(Some(text)) => {
            let _ = updates.send(Update::Completed(text));
        }
        Ok(None) => {}
        Err(error) => {
            let _ = updates.send(Update::Failed(error));
        }
    }
    let _ = updates.send(Update::Ended);
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

struct Item {
    id: String,
    commands: Option<mpsc::Sender<Cmd>>,
    updates: UnboundedReceiver<Update>,
    decoder: PcmStream,
    committed: String,
    committed_by_client: bool,
    /// Set for a translation turn on a model that cannot stream: the audio is
    /// collected here until the client commits (see [`BufferedTurn`]).
    buffered: Option<BufferedTurn>,
}

impl Item {
    /// Hand audio to the stream worker, or collect it for the buffered turn.
    fn feed(&mut self, pcm: Vec<f32>) {
        if pcm.is_empty() {
            return;
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(Cmd::Feed(pcm));
        } else if let Some(turn) = &mut self.buffered {
            turn.pcm.extend(pcm);
        }
    }

    /// The client committed: end the stream, or run the buffered turn.
    fn finalize(&mut self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Cmd::Finalize);
        } else if let Some(turn) = self.buffered.take() {
            tokio::spawn(run_buffered_turn(turn));
        }
    }
}

struct Closed;

struct Connection {
    app: std::sync::Arc<App>,
    socket: WebSocket,
    config: SessionConfig,
    item: Option<Item>,
    previous_item_id: Option<String>,
}

enum Flow {
    Continue,
    Close,
}

type Handled = Result<Flow, Closed>;

impl Connection {
    fn new(app: std::sync::Arc<App>, socket: WebSocket, model: Option<String>) -> Self {
        Self {
            app,
            socket,
            config: SessionConfig::new(model),
            item: None,
            previous_item_id: None,
        }
    }

    async fn send(&mut self, value: Value) -> Result<(), Closed> {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .map_err(|_| Closed)
    }

    async fn send_error(&mut self, error: &ApiError) -> Result<(), Closed> {
        self.send(error_event(error)).await
    }

    async fn run(&mut self) -> Result<(), Closed> {
        let created = event(
            "transcription_session.created",
            json!({"session": self.config.to_json()}),
        );
        self.send(created).await?;
        loop {
            let idle = if self.item.is_some() {
                IDLE_TIMEOUT
            } else {
                Duration::from_secs(24 * 60 * 60)
            };
            tokio::select! {
                message = tokio::time::timeout(idle, self.socket.recv()) => {
                    let Ok(message) = message else {
                        self.discard_item().await;
                        self.send_error(&ApiError::new(
                            StatusCode::REQUEST_TIMEOUT,
                            "idle_timeout",
                            "No audio or commit received; the buffered audio was discarded",
                        )).await?;
                        continue;
                    };
                    let flow = match message {
                        Some(Ok(Message::Text(text))) => self.on_text(text.as_str()).await?,
                        Some(Ok(Message::Binary(_))) => {
                            self.send_error(&invalid(
                                "invalid_event",
                                "Binary frames are not accepted; send JSON events",
                            ))
                            .await?;
                            Flow::Continue
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => Flow::Close,
                        Some(Ok(_)) => Flow::Continue,
                    };
                    if matches!(flow, Flow::Close) {
                        self.discard_item().await;
                        return Ok(());
                    }
                }
                update = next_update(&mut self.item) => {
                    self.on_update(update).await?;
                }
            }
        }
    }

    async fn on_text(&mut self, text: &str) -> Handled {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            self.send_error(&invalid("invalid_json", "The event is not valid JSON"))
                .await?;
            return Ok(Flow::Continue);
        };
        match message.get("type").and_then(Value::as_str) {
            Some(kind @ ("transcription_session.update" | "session.update")) => {
                let updated = if kind == "session.update" {
                    "session.updated"
                } else {
                    "transcription_session.updated"
                };
                match self
                    .config
                    .update(message.get("session").unwrap_or(&Value::Null))
                {
                    Ok(()) => {
                        let body = event(updated, json!({"session": self.config.to_json()}));
                        self.send(body).await?;
                    }
                    Err(error) => self.send_error(&error).await?,
                }
                Ok(Flow::Continue)
            }
            Some("input_audio_buffer.append") => self.on_append(&message).await,
            Some("input_audio_buffer.commit") => self.on_commit().await,
            Some("input_audio_buffer.clear") => self.on_clear().await,
            _ => {
                self.send_error(&invalid("invalid_event", "Unknown or missing event type"))
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    async fn on_append(&mut self, message: &Value) -> Handled {
        let decoded = message
            .get("audio")
            .and_then(Value::as_str)
            .and_then(|audio| base64::engine::general_purpose::STANDARD.decode(audio).ok());
        let Some(bytes) = decoded else {
            self.send_error(&invalid(
                "invalid_audio",
                "audio must be base64-encoded PCM16",
            ))
            .await?;
            return Ok(Flow::Continue);
        };
        // A committed item is still finishing: the new audio starts the next one.
        self.settle().await?;
        if self.item.is_none() {
            match start_item(&self.app, &self.config).await {
                Ok(item) => self.item = Some(item),
                Err(error) => {
                    self.send_error(&error).await?;
                    return Ok(if error.code == "streaming_unsupported" {
                        Flow::Close
                    } else {
                        Flow::Continue
                    });
                }
            }
        }
        let item = self.item.as_mut().expect("item was just started");
        match item.decoder.push(&bytes) {
            Ok(pcm) => item.feed(pcm),
            Err(error) => self.send_error(&error).await?,
        }
        Ok(Flow::Continue)
    }

    async fn on_commit(&mut self) -> Handled {
        self.settle().await?;
        let Some(item) = self.item.as_mut() else {
            self.send_error(&invalid(
                "input_audio_buffer_commit_empty",
                "There is no audio to commit",
            ))
            .await?;
            return Ok(Flow::Continue);
        };
        let tail = item.decoder.finish();
        if let Ok(pcm) = &tail {
            item.feed(pcm.clone());
            item.finalize();
        }
        item.committed_by_client = true;
        let committed = event(
            "input_audio_buffer.committed",
            json!({"previous_item_id": self.previous_item_id, "item_id": item.id}),
        );
        match tail {
            Ok(_) => self.send(committed).await?,
            Err(error) => self.send_error(&error).await?,
        }
        Ok(Flow::Continue)
    }

    async fn on_clear(&mut self) -> Handled {
        self.settle().await?;
        self.discard_item().await;
        self.send(event("input_audio_buffer.cleared", json!({})))
            .await?;
        Ok(Flow::Continue)
    }

    async fn on_update(&mut self, update: Option<Update>) -> Result<(), Closed> {
        let Some(update) = update else {
            self.item = None;
            return Ok(());
        };
        let Some(item) = self.item.as_mut() else {
            return Ok(());
        };
        let item_id = item.id.clone();
        match update {
            Update::Partial {
                committed,
                tentative,
                revision,
            } => {
                let delta = delta_since(&item.committed, &committed).map(str::to_owned);
                if committed.starts_with(&item.committed) {
                    item.committed.clone_from(&committed);
                }
                if let Some(delta) = delta {
                    let body = event(
                        "conversation.item.input_audio_transcription.delta",
                        json!({"item_id": item_id, "content_index": 0, "delta": delta}),
                    );
                    self.send(body).await?;
                }
                let body = event(
                    "conversation.item.input_audio_transcription.partial",
                    json!({
                        "item_id": item_id,
                        "content_index": 0,
                        "committed": committed,
                        "tentative": tentative,
                        "revision": revision,
                    }),
                );
                self.send(body).await?;
            }
            Update::Completed(transcript) => {
                self.previous_item_id = Some(item_id.clone());
                let body = event(
                    "conversation.item.input_audio_transcription.completed",
                    json!({"item_id": item_id, "content_index": 0, "transcript": transcript}),
                );
                self.send(body).await?;
            }
            Update::Failed(error) => self.send_error(&error).await?,
            Update::Ended => self.item = None,
        }
        Ok(())
    }

    /// Wait for a committed item to finish (its `Ended` update).
    async fn settle(&mut self) -> Result<(), Closed> {
        while self.item.as_ref().is_some_and(|i| i.committed_by_client) {
            let update = next_update(&mut self.item).await;
            self.on_update(update).await?;
        }
        Ok(())
    }

    /// Abandon the in-flight item and wait for its worker to release the slot.
    async fn discard_item(&mut self) {
        if let Some(mut item) = self.item.take() {
            item.commands = None;
            if item.buffered.is_some() {
                // Never committed, so no worker is running: nothing to wait for.
                return;
            }
            while let Some(update) = item.updates.recv().await {
                if matches!(update, Update::Ended) {
                    break;
                }
            }
        }
    }
}

/// A translation turn on a model that cannot stream (owner decision D1): the
/// turn's audio is buffered and, once the client commits, translated by one
/// batch run. Only that run takes the batch permit.
struct BufferedTurn {
    app: std::sync::Arc<App>,
    model: String,
    run: RunOptions,
    pcm: Vec<f32>,
    stream_slot: tokio::sync::OwnedSemaphorePermit,
    updates: UnboundedSender<Update>,
}

/// Translate the buffered turn and report it as one full-turn delta followed
/// by `completed`.
async fn run_buffered_turn(turn: BufferedTurn) {
    let BufferedTurn {
        app,
        model,
        run,
        pcm,
        stream_slot,
        updates,
    } = turn;
    let outcome = translate_pcm(&app, &model, run, pcm).await;
    // Free the slot before announcing the end, as `stream_worker` does.
    drop(stream_slot);
    match outcome {
        Ok(text) => {
            if !text.is_empty() {
                let _ = updates.send(Update::Partial {
                    committed: text.clone(),
                    tentative: String::new(),
                    revision: 0,
                });
            }
            let _ = updates.send(Update::Completed(text));
        }
        Err(error) => {
            let _ = updates.send(Update::Failed(error));
        }
    }
    let _ = updates.send(Update::Ended);
}

/// One batch run of `pcm` against `model_id`, holding the batch permit only
/// for the run itself.
async fn translate_pcm(
    app: &App,
    model_id: &str,
    run: RunOptions,
    pcm: Vec<f32>,
) -> Result<String, ApiError> {
    if pcm.is_empty() {
        return Ok(String::new());
    }
    let limits = *app.limits.read().map_err(internal)?;
    let wait_timeout = limits.queue_wait_timeout_ms.map(Duration::from_millis);
    let (permit, _) = app
        .inference
        .acquire(limits.queue_max_waiting, wait_timeout)
        .await
        .map_err(queue_api_error)?;
    let (model, _, _, _) = bind_or_swap_model(app, model_id, wait_timeout).await?;
    let transcript = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        model.session()?.run(&pcm, &run)
    })
    .await
    .map_err(internal)?
    .map_err(map_engine_error)?;
    Ok(transcript.text.trim().to_owned())
}

/// Claim the stream slot, bind the model, and start the worker for a new item.
async fn start_item(app: &std::sync::Arc<App>, config: &SessionConfig) -> Result<Item, ApiError> {
    let stream_slot = app.inference.try_acquire_stream().ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "stream_busy",
            "Another live stream is already running",
        )
    })?;
    let target = resolve_requested_model(app, &config.model)?;
    require_installed_verified(app, &target)?;
    let limits = *app.limits.read().map_err(internal)?;
    let wait_timeout = limits.queue_wait_timeout_ms.map(Duration::from_millis);
    let (batch_permit, _) = app
        .inference
        .acquire(limits.queue_max_waiting, wait_timeout)
        .await
        .map_err(queue_api_error)?;
    let (model, _, _, caps) = bind_or_swap_model(app, &target, wait_timeout).await?;
    let translating = config.target_language.is_some();
    let buffered = translating && !caps.loaded.supports_streaming;
    if !buffered {
        check_streaming(&caps)?;
    }
    let request = ParsedRequest {
        language: config.language.clone(),
        prompt: config.prompt.clone(),
        temperature: None,
        response_format: None,
        timestamp_granularities: Vec::new(),
        target_language: config.target_language.clone(),
    };
    let endpoint = if translating {
        Endpoint::Translations
    } else {
        Endpoint::Transcriptions
    };
    let run = plan(&request, &caps, endpoint, None)?.to_run_options();
    let decoder = PcmStream::new(config.sample_rate)?;
    let (update_tx, update_rx) = unbounded_channel();
    let mut item = Item {
        id: format!("item_{}", Uuid::new_v4().simple()),
        commands: None,
        updates: update_rx,
        decoder,
        committed: String::new(),
        committed_by_client: false,
        buffered: None,
    };

    if buffered {
        // Nothing runs until the commit, so the batch queue moves on now.
        drop(batch_permit);
        item.buffered = Some(BufferedTurn {
            app: app.clone(),
            model: target,
            run,
            pcm: Vec::new(),
            stream_slot,
            updates: update_tx,
        });
        return Ok(item);
    }
    let (command_tx, command_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("realtime-stream".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || stream_worker(model, run, batch_permit, stream_slot, command_rx, update_tx))
        .map_err(internal)?;
    item.commands = Some(command_tx);
    Ok(item)
}

/// The next update from the in-flight item (`None`: its worker vanished);
/// pends forever when there is no item.
async fn next_update(item: &mut Option<Item>) -> Option<Update> {
    match item {
        Some(item) => item.updates.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{LoadedCaps, TimestampGranularity};

    #[test]
    fn delta_is_the_newly_committed_suffix() {
        assert_eq!(delta_since("", "Hello"), Some("Hello"));
        assert_eq!(delta_since("Hello", "Hello world"), Some(" world"));
        assert_eq!(delta_since("Hello", "Hello"), None);
        assert_eq!(delta_since("Hello", "Help"), None);
    }

    #[test]
    fn session_update_reads_the_beta_shape() {
        let mut config = SessionConfig::new(None);
        config
            .update(&json!({
                "input_audio_format": "pcm16",
                "input_audio_sample_rate": 16000,
                "input_audio_transcription": {"model": "m", "language": "pl", "prompt": "x"},
                "turn_detection": null,
            }))
            .unwrap();
        assert_eq!(config.sample_rate, 16_000);
        assert_eq!(config.model, "m");
        assert_eq!(config.language.as_deref(), Some("pl"));
        assert_eq!(config.prompt.as_deref(), Some("x"));
    }

    #[test]
    fn session_update_reads_the_ga_shape() {
        let mut config = SessionConfig::new(None);
        config
            .update(&json!({"type": "transcription", "audio": {"input": {
                "format": {"type": "audio/pcm", "rate": 16000},
                "transcription": {"model": "m", "languages": ["de", "en"]},
                "turn_detection": null,
            }}}))
            .unwrap();
        assert_eq!(config.sample_rate, 16_000);
        assert_eq!(config.language.as_deref(), Some("de"));
    }

    #[test]
    fn session_update_rejects_unsupported_settings_without_changing_anything() {
        let mut config = SessionConfig::new(None);
        let before = config.clone();
        for session in [
            json!({"turn_detection": {"type": "server_vad"}}),
            json!({"input_audio_format": "g711_ulaw"}),
            json!({"input_audio_sample_rate": 1}),
            json!({"input_audio_transcription": {"model": "m"}, "input_audio_format": "x"}),
        ] {
            assert!(config.update(&session).is_err(), "{session}");
            assert_eq!(config, before);
        }
    }

    #[test]
    fn a_model_without_streaming_is_refused_with_the_documented_code() {
        let caps = |supports_streaming| {
            EffectiveCaps::new(LoadedCaps {
                arch: "whisper".to_owned(),
                languages: vec!["en".to_owned()],
                translate_target_languages: vec![],
                supports_translate: false,
                supports_language_detect: false,
                max_timestamp_kind: TimestampGranularity::None,
                feature_initial_prompt_flag: false,
                whisper_ext_accepted: None,
                prompt_max_tokens: None,
                timestamp_granularity_rejected: false,
                supports_streaming,
                ..Default::default()
            })
        };
        assert!(check_streaming(&caps(true)).is_ok());
        let error = check_streaming(&caps(false)).unwrap_err();
        assert_eq!(error.code, "streaming_unsupported");
        assert_eq!(
            error_event(&error)["error"]["code"],
            "streaming_unsupported"
        );
    }

    // -- Protocol over a real socket -----------------------------------------

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as ClientMessage};

    struct TestServer {
        app: std::sync::Arc<App>,
        address: std::net::SocketAddr,
        path: std::path::PathBuf,
        server: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start() -> Self {
            let parent = std::env::temp_dir().canonicalize().unwrap();
            let path = parent.join(format!("stt-server-test-{}", Uuid::new_v4()));
            let app = crate::app::open_app_at(path.clone()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = crate::api::router(app.clone());
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap();
            });
            Self {
                app,
                address,
                path,
                server,
            }
        }

        fn request(
            &self,
            query: &str,
            token: Option<&str>,
        ) -> tokio_tungstenite::tungstenite::http::Request<()> {
            let mut request = format!("ws://{}/v1/realtime?{query}", self.address)
                .into_client_request()
                .unwrap();
            if let Some(token) = token {
                request
                    .headers_mut()
                    .insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            request
        }

        /// Stop serving and delete the data dir (Windows keeps files locked
        /// until every connection task has dropped its `App` handle).
        async fn finish(self) {
            let Self {
                app, path, server, ..
            } = self;
            server.abort();
            let _ = server.await;
            drop(app);
            let path = path.canonicalize().unwrap();
            for _ in 0..100 {
                if std::fs::remove_dir_all(&path).is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            std::fs::remove_dir_all(&path).unwrap();
        }
    }

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn next_event(client: &mut Client) -> Value {
        loop {
            if let ClientMessage::Text(text) = client.next().await.unwrap().unwrap() {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    async fn call(client: &mut Client, body: Value) -> Value {
        client
            .send(ClientMessage::Text(body.to_string().into()))
            .await
            .unwrap();
        next_event(client).await
    }

    fn http_status(error: tokio_tungstenite::tungstenite::Error) -> StatusCode {
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
            other => panic!("expected an HTTP response, got {other}"),
        }
    }

    #[tokio::test]
    async fn connecting_without_a_token_is_unauthorized() {
        let server = TestServer::start().await;
        let error = tokio_tungstenite::connect_async(server.request("intent=transcription", None))
            .await
            .unwrap_err();
        assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
        server.finish().await;
    }

    #[tokio::test]
    async fn an_unknown_intent_is_rejected_before_the_upgrade() {
        let server = TestServer::start().await;
        let token = server.app.token.clone();
        let error =
            tokio_tungstenite::connect_async(server.request("intent=conversation", Some(&token)))
                .await
                .unwrap_err();
        assert_eq!(http_status(error), StatusCode::BAD_REQUEST);
        server.finish().await;
    }

    #[tokio::test]
    async fn a_user_token_connects_and_the_session_events_flow() {
        let server = TestServer::start().await;
        let user_token = server.app.user_token.clone();
        let (mut client, _) = tokio_tungstenite::connect_async(
            server.request("intent=transcription", Some(&user_token)),
        )
        .await
        .unwrap();

        let created = next_event(&mut client).await;
        assert_eq!(created["type"], "transcription_session.created");
        assert_eq!(created["session"]["input_audio_format"], "pcm16");
        assert_eq!(created["session"]["input_audio_sample_rate"], 24_000);
        assert!(created["session"]["turn_detection"].is_null());

        let updated = call(
            &mut client,
            json!({"type": "transcription_session.update", "session": {
                "input_audio_sample_rate": 16000,
                "input_audio_transcription": {"language": "en"},
            }}),
        )
        .await;
        assert_eq!(updated["type"], "transcription_session.updated");
        assert_eq!(updated["session"]["input_audio_sample_rate"], 16_000);
        assert_eq!(
            updated["session"]["input_audio_transcription"]["language"],
            "en"
        );

        let ga = call(
            &mut client,
            json!({"type": "session.update", "session": {"type": "transcription"}}),
        )
        .await;
        assert_eq!(ga["type"], "session.updated");

        let vad = call(
            &mut client,
            json!({"type": "transcription_session.update",
                   "session": {"turn_detection": {"type": "server_vad"}}}),
        )
        .await;
        assert_eq!(vad["type"], "error");
        assert_eq!(vad["error"]["code"], "turn_detection_unsupported");

        let empty = call(&mut client, json!({"type": "input_audio_buffer.commit"})).await;
        assert_eq!(empty["error"]["code"], "input_audio_buffer_commit_empty");

        let unknown = call(&mut client, json!({"type": "nope"})).await;
        assert_eq!(unknown["error"]["code"], "invalid_event");

        let bad_audio = call(
            &mut client,
            json!({"type": "input_audio_buffer.append", "audio": "***"}),
        )
        .await;
        assert_eq!(bad_audio["error"]["code"], "invalid_audio");

        // No model is selected in a fresh data dir: the item cannot start, the
        // error is reported, and the connection stays usable.
        let audio = base64::engine::general_purpose::STANDARD.encode([0u8; 640]);
        let no_model = call(
            &mut client,
            json!({"type": "input_audio_buffer.append", "audio": audio}),
        )
        .await;
        assert_eq!(no_model["type"], "error");
        assert_eq!(no_model["error"]["code"], "server_not_ready");
        let cleared = call(&mut client, json!({"type": "input_audio_buffer.clear"})).await;
        assert_eq!(cleared["type"], "input_audio_buffer.cleared");

        drop(client);
        server.finish().await;
    }

    #[test]
    fn session_update_reads_the_target_language_in_both_shapes() {
        let mut config = SessionConfig::new(None);
        assert_eq!(config.target_language, None);
        config
            .update(&json!({"input_audio_transcription": {"target_language": "de"}}))
            .unwrap();
        assert_eq!(config.target_language.as_deref(), Some("de"));
        assert_eq!(
            config.to_json()["input_audio_transcription"]["target_language"],
            "de"
        );

        // OpenAI's GA name for the output language.
        config
            .update(&json!({"type": "transcription", "audio": {"output": {"language": "fr"}}}))
            .unwrap();
        assert_eq!(config.target_language.as_deref(), Some("fr"));

        // An update that does not mention it keeps it; an empty value clears it.
        config
            .update(&json!({"input_audio_sample_rate": 16000}))
            .unwrap();
        assert_eq!(config.target_language.as_deref(), Some("fr"));
        config
            .update(&json!({"input_audio_transcription": {"target_language": ""}}))
            .unwrap();
        assert_eq!(config.target_language, None);
        assert!(config.to_json()["input_audio_transcription"]["target_language"].is_null());
    }

    #[test]
    fn translation_session_errors_are_reported_as_error_events() {
        let caps = |supports_translate, targets: &[&str]| {
            EffectiveCaps::new(LoadedCaps {
                arch: "whisper".to_owned(),
                languages: vec!["en".to_owned(), "es".to_owned()],
                translate_target_languages: targets.iter().map(|t| t.to_string()).collect(),
                supports_translate,
                supports_language_detect: false,
                max_timestamp_kind: TimestampGranularity::None,
                ..Default::default()
            })
        };
        let plan_error = |caps: &EffectiveCaps, target: &str| {
            let request = ParsedRequest {
                language: Some("es".to_owned()),
                target_language: Some(target.to_owned()),
                ..Default::default()
            };
            error_event(&plan(&request, caps, Endpoint::Translations, None).unwrap_err())
        };
        let none = plan_error(&caps(false, &[]), "en");
        assert_eq!(none["type"], "error");
        assert_eq!(none["error"]["code"], "unsupported_capability");
        let wrong_target = plan_error(&caps(true, &["en", "de"]), "fr");
        assert_eq!(wrong_target["error"]["code"], "unsupported_target_language");
        assert_eq!(wrong_target["error"]["type"], "invalid_request_error");
    }

    fn base64_pcm16(wav_path: &str) -> Vec<String> {
        let mut reader = hound::WavReader::open(wav_path).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        let bytes: Vec<u8> = reader
            .samples::<i16>()
            .flat_map(|s| s.unwrap().to_le_bytes())
            .collect();
        bytes
            .chunks(32_000)
            .map(|chunk| base64::engine::general_purpose::STANDARD.encode(chunk))
            .collect()
    }

    /// Runs a translation turn end to end over a real socket: a model that
    /// cannot stream, the audio buffered, one batch run on commit, then one
    /// full-turn delta and `completed`. Needs a local non-streaming model that
    /// translates (any whisper gguf; it is only read) and a 16 kHz mono wav,
    /// so it is opt-in:
    ///
    /// ```text
    /// STT_TEST_TRANSLATE_MODEL=<whisper gguf>
    /// STT_TEST_TRANSLATE_WAV=<16 kHz mono 16-bit wav>
    /// cargo test realtime_translation_turn -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs STT_TEST_TRANSLATE_MODEL and STT_TEST_TRANSLATE_WAV"]
    async fn realtime_translation_turn_runs_one_batch_translate_on_commit() {
        let model_path = std::env::var("STT_TEST_TRANSLATE_MODEL").expect("model path");
        let wav_path = std::env::var("STT_TEST_TRANSLATE_WAV").expect("wav path");
        let (model, diagnostic) =
            crate::engine::load_engine(std::path::Path::new(&model_path), "cpu")
                .expect("model loads");
        let capabilities = model.capabilities();
        assert!(
            !capabilities.supports_streaming,
            "use a non-streaming model"
        );
        assert!(capabilities.supports_translate, "use a translating model");

        let server = TestServer::start().await;
        let user_token = server.app.user_token.clone();
        server
            .app
            .db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO installed(id,path,sha256,source) VALUES('turn-model','unused','x','catalog_download')",
                [],
            )
            .unwrap();
        *server.app.loaded.lock().unwrap() = Some(crate::engine::LoadedModel::new(
            "turn-model".to_owned(),
            model,
            diagnostic,
        ));

        let (mut client, _) = tokio_tungstenite::connect_async(
            server.request("intent=transcription&model=turn-model", Some(&user_token)),
        )
        .await
        .unwrap();
        next_event(&mut client).await;
        let updated = call(
            &mut client,
            json!({"type": "transcription_session.update", "session": {
                "input_audio_sample_rate": 16000,
                "input_audio_transcription": {"target_language": "en"},
            }}),
        )
        .await;
        assert_eq!(
            updated["session"]["input_audio_transcription"]["target_language"],
            "en"
        );

        for chunk in base64_pcm16(&wav_path) {
            client
                .send(ClientMessage::Text(
                    json!({"type": "input_audio_buffer.append", "audio": chunk})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        client
            .send(ClientMessage::Text(
                json!({"type": "input_audio_buffer.commit"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

        let mut kinds = Vec::new();
        let mut delta = None;
        let mut transcript = None;
        while transcript.is_none() {
            let event = next_event(&mut client).await;
            let kind = event["type"].as_str().unwrap().to_owned();
            assert_ne!(kind, "error", "{event}");
            match kind.as_str() {
                "conversation.item.input_audio_transcription.delta" => {
                    delta = event["delta"].as_str().map(str::to_owned);
                }
                "conversation.item.input_audio_transcription.completed" => {
                    transcript = event["transcript"].as_str().map(str::to_owned);
                }
                _ => {}
            }
            kinds.push(kind);
        }
        println!(
            "events: {kinds:?}
transcript: {transcript:?}"
        );
        let transcript = transcript.unwrap();
        assert!(!transcript.is_empty());
        assert_eq!(
            delta.as_deref(),
            Some(transcript.as_str()),
            "one full-turn delta"
        );
        assert_eq!(kinds[0], "input_audio_buffer.committed");

        drop(client);
        server.finish().await;
    }

    /// Streams a real WAV through the worker with a real streaming model.
    /// Needs a local model, so it is opt-in:
    ///
    /// ```text
    /// STT_TEST_STREAMING_MODEL=<parakeet-unified or nemotron gguf>
    /// STT_TEST_STREAMING_WAV=<16 kHz mono 16-bit wav>
    /// cargo test --release realtime_worker -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs STT_TEST_STREAMING_MODEL and STT_TEST_STREAMING_WAV"]
    fn realtime_worker_streams_a_wav_through_a_streaming_model() {
        let model_path = std::env::var("STT_TEST_STREAMING_MODEL").expect("model path");
        let wav_path = std::env::var("STT_TEST_STREAMING_WAV").expect("wav path");
        let (model, _) = crate::engine::load_engine(std::path::Path::new(&model_path), "auto")
            .expect("model loads");
        assert!(
            model.capabilities().supports_streaming,
            "not a streaming model"
        );
        let mut reader = hound::WavReader::open(wav_path).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| f32::from(s.unwrap()) / 32768.0)
            .collect();

        let queue = crate::queue::InferenceQueue::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (permit, _) = runtime.block_on(queue.acquire(None, None)).unwrap();
        let slot = queue.try_acquire_stream().unwrap();
        assert!(queue.try_acquire_stream().is_none(), "one stream at a time");
        let (command_tx, command_rx) = mpsc::channel();
        let (update_tx, mut update_rx) = unbounded_channel();
        let worker = std::thread::spawn(move || {
            stream_worker(
                model,
                RunOptions::default(),
                permit,
                slot,
                command_rx,
                update_tx,
            )
        });
        for chunk in samples.chunks(1600) {
            command_tx.send(Cmd::Feed(chunk.to_vec())).unwrap();
        }
        command_tx.send(Cmd::Finalize).unwrap();
        let mut partials = 0;
        let mut transcript = None;
        while let Some(update) = update_rx.blocking_recv() {
            match update {
                Update::Partial { .. } => partials += 1,
                Update::Completed(text) => transcript = Some(text),
                Update::Failed(error) => panic!("stream failed: {}", error.message),
                Update::Ended => break,
            }
        }
        worker.join().unwrap();
        assert!(partials > 0);
        let transcript = transcript.expect("a completed transcript");
        println!("transcript: {transcript:?}");
        assert!(!transcript.is_empty());
        assert!(
            queue.try_acquire_stream().is_some(),
            "slot released at the end"
        );
    }
}
