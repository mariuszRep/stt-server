use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sherpa_manifest::{ModelEntry, ModelFiles};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
    OfflineTransducerModelConfig, OfflineWhisperModelConfig,
};
use tokio::sync::{mpsc, oneshot};

const MAX_BATCH_SIZE: usize = 8;
const MAX_WAIT: Duration = Duration::from_millis(25);
const QUEUE_CAPACITY: usize = 64;

pub struct TranscribeRequest {
    pub samples: Vec<f32>,
    pub sample_rate: i32,
}

#[derive(Debug, Clone, Default)]
pub struct TranscribeResponse {
    pub text: String,
    pub tokens: Vec<String>,
    pub timestamps: Option<Vec<f32>>,
    pub durations: Option<Vec<f32>>,
}

pub struct Job {
    pub request: TranscribeRequest,
    pub respond_to: oneshot::Sender<TranscribeResponse>,
}

fn path_str(dir: &Path, rel: &str) -> String {
    dir.join(rel).to_string_lossy().into_owned()
}

/// Build an `OfflineRecognizerConfig` for `entry` from files already
/// extracted at `install_dir`, per its model family.
pub fn build_config(
    entry: &ModelEntry,
    install_dir: &Path,
    num_threads: i32,
) -> OfflineRecognizerConfig {
    let mut config = OfflineRecognizerConfig::default();

    match &entry.files {
        ModelFiles::SenseVoice { model, tokens } => {
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: Some(path_str(install_dir, model)),
                language: Some(entry.default_language.to_string()),
                use_itn: true,
            };
            config.model_config.tokens = Some(path_str(install_dir, tokens));
        }
        ModelFiles::Whisper {
            encoder,
            decoder,
            tokens,
        } => {
            config.model_config.whisper = OfflineWhisperModelConfig {
                encoder: Some(path_str(install_dir, encoder)),
                decoder: Some(path_str(install_dir, decoder)),
                language: Some(entry.default_language.to_string()),
                task: Some("transcribe".to_string()),
                tail_paddings: -1,
                enable_token_timestamps: true,
                enable_segment_timestamps: false,
            };
            config.model_config.tokens = Some(path_str(install_dir, tokens));
        }
        ModelFiles::Transducer {
            encoder,
            decoder,
            joiner,
            tokens,
        } => {
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: Some(path_str(install_dir, encoder)),
                decoder: Some(path_str(install_dir, decoder)),
                joiner: Some(path_str(install_dir, joiner)),
            };
            config.model_config.tokens = Some(path_str(install_dir, tokens));
        }
    }

    config.model_config.num_threads = num_threads;
    config.model_config.debug = false;
    config.model_config.provider = Some("cpu".to_string());
    config
}

/// Spawn the per-model worker task: batches incoming jobs (up to
/// MAX_BATCH_SIZE, or whatever arrives within MAX_WAIT of the first job in a
/// batch) and decodes them in one `decode_multiple_streams` call, mirroring
/// sherpa-onnx's own reference server's queueing pattern
/// (python-api-examples/non_streaming_server.py).
pub fn spawn_worker(recognizer: OfflineRecognizer) -> mpsc::Sender<Job> {
    let (tx, mut rx) = mpsc::channel::<Job>(QUEUE_CAPACITY);
    let recognizer = Arc::new(recognizer);

    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            let deadline = Instant::now() + MAX_WAIT;

            while batch.len() < MAX_BATCH_SIZE {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Some(job)) => batch.push(job),
                    _ => break,
                }
            }

            let recognizer = recognizer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let streams: Vec<_> = batch
                    .iter()
                    .map(|job| {
                        let stream = recognizer.create_stream();
                        stream.accept_waveform(job.request.sample_rate, &job.request.samples);
                        stream
                    })
                    .collect();
                let refs: Vec<_> = streams.iter().collect();
                recognizer.decode_multiple_streams(&refs);

                for (job, stream) in batch.into_iter().zip(streams.iter()) {
                    let response = match stream.get_result() {
                        Some(r) => TranscribeResponse {
                            text: r.text,
                            tokens: r.tokens,
                            timestamps: r.timestamps,
                            durations: r.durations,
                        },
                        None => TranscribeResponse::default(),
                    };
                    let _ = job.respond_to.send(response);
                }
            })
            .await;
        }
    });

    tx
}
