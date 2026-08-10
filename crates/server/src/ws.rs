use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use tracing::{info, warn};

use stt_adapter::EngineAdapter;
use stt_common::{
    RealtimeClientMessage, RealtimeServerMessage, SampleFormat, SessionId,
};

use crate::state::AppState;

pub async fn ws_handler<A: EngineAdapter + 'static>(
    ws: WebSocketUpgrade,
    State(state): State<AppState<A>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket<A: EngineAdapter + 'static>(mut socket: WebSocket, state: AppState<A>) {
    let session_id = SessionId::new();
    info!("WebSocket session {session_id} connected");

    let mut ctx = None;

    while let Some(msg) = socket.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<RealtimeClientMessage>(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Invalid message from client: {e}");
                        let err = RealtimeServerMessage::Error {
                            code: "INVALID_MESSAGE".into(),
                            message: format!("invalid message format: {e}"),
                        };
                        let _ = socket
                            .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                            .await;
                        continue;
                    }
                }
            }
            Ok(Message::Binary(data)) => RealtimeClientMessage::Binary {
                data: data.to_vec(),
            },
            Ok(Message::Close(_)) => {
                info!("WebSocket session {session_id} closed by client");
                break;
            }
            Err(e) => {
                warn!("WebSocket error: {e}");
                break;
            }
            _ => continue,
        };

        match msg {
            RealtimeClientMessage::Start { config } => {
                info!("Session {session_id}: start with config {:?}", config);

                if config.sample_format != SampleFormat::Signed16BitLittleEndian {
                    let err = RealtimeServerMessage::Error {
                        code: "UNSUPPORTED_FORMAT".into(),
                        message: "only 16kHz mono signed 16-bit little-endian PCM is supported"
                            .into(),
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                    continue;
                }

                let model_id = match state.adapter.get_selected_model().await {
                    Ok(Some(id)) => id,
                    _ => {
                        let err = RealtimeServerMessage::Error {
                            code: "NO_MODEL_SELECTED".into(),
                            message: "no model specified and no default selected".into(),
                        };
                        let _ = socket
                            .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                            .await;
                        continue;
                    }
                };

                match state
                    .adapter
                    .create_realtime_context(
                        model_id,
                        config.sample_rate,
                        config.language.as_deref(),
                    )
                    .await
                {
                    Ok(new_ctx) => {
                        let started = RealtimeServerMessage::Started {
                            session_id: new_ctx.session_id,
                        };
                        let _ = socket
                            .send(Message::Text(serde_json::to_string(&started).unwrap().into()))
                            .await;
                        ctx = Some(new_ctx);
                    }
                    Err(e) => {
                        let err = RealtimeServerMessage::Error {
                            code: "CONTEXT_ERROR".into(),
                            message: format!("failed to create context: {e}"),
                        };
                        let _ = socket
                            .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                            .await;
                    }
                }
            }

            RealtimeClientMessage::Binary { data } => {
                if let Some(ref mut realtime_ctx) = ctx {
                    let samples = bytes_to_f32_samples(&data);

                    match state.adapter.feed_realtime_audio(realtime_ctx, &samples).await {
                        Ok(partials) => {
                            for text in partials {
                                let msg = RealtimeServerMessage::Partial { text };
                                let _ = socket
                                    .send(Message::Text(
                                        serde_json::to_string(&msg).unwrap().into(),
                                    ))
                                    .await;
                            }
                        }
                        Err(e) => {
                            let err = RealtimeServerMessage::Error {
                                code: "AUDIO_ERROR".into(),
                                message: format!("audio processing failed: {e}"),
                            };
                            let _ = socket
                                .send(Message::Text(
                                    serde_json::to_string(&err).unwrap().into(),
                                ))
                                .await;
                        }
                    }
                } else {
                    let err = RealtimeServerMessage::Error {
                        code: "NO_SESSION".into(),
                        message: "no active session; send 'start' first".into(),
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                }
            }

            RealtimeClientMessage::Complete => {
                if let Some(mut realtime_ctx) = ctx.take() {
                    match state.adapter.finalize_realtime(&mut realtime_ctx).await {
                        Ok(result) => {
                            let msg = RealtimeServerMessage::Final {
                                text: result.text,
                                segments: result.segments,
                            };
                            let _ = socket
                                .send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
                                .await;

                            let completed = RealtimeServerMessage::Completed {
                                session_id: realtime_ctx.session_id,
                            };
                            let _ = socket
                                .send(Message::Text(
                                    serde_json::to_string(&completed).unwrap().into(),
                                ))
                                .await;

                            let _ = state.adapter.destroy_realtime_context(realtime_ctx).await;
                        }
                        Err(e) => {
                            let err = RealtimeServerMessage::Error {
                                code: "FINALIZE_ERROR".into(),
                                message: format!("finalize failed: {e}"),
                            };
                            let _ = socket
                                .send(Message::Text(
                                    serde_json::to_string(&err).unwrap().into(),
                                ))
                                .await;
                        }
                    }
                }
            }

            RealtimeClientMessage::Cancel => {
                if let Some(realtime_ctx) = ctx.take() {
                    let _ = state.adapter.destroy_realtime_context(realtime_ctx).await;
                    let completed = RealtimeServerMessage::Completed {
                        session_id: SessionId::new(),
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&completed).unwrap().into()))
                        .await;
                }
            }
        }
    }

    if let Some(realtime_ctx) = ctx {
        let _ = state.adapter.destroy_realtime_context(realtime_ctx).await;
    }

    info!("WebSocket session {session_id} ended");
}

/// Convert raw bytes (s16le PCM) to f32 samples normalized to [-1, 1].
fn bytes_to_f32_samples(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / 32768.0
        })
        .collect()
}
