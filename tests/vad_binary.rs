//! Offline speech detection against the real compiled binary: no model is installed or loaded,
//! yet `POST /v1/audio/vad` finds the speech in the fixture and `/v1/local/system` advertises
//! the engine.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use stt_server::discovery;

const EXE: &str = env!("CARGO_BIN_EXE_stt-server");
const SPEECH_WAV: &[u8] = include_bytes!("fixtures/speech-16k.wav");

struct ChildGuard {
    child: Child,
    data_dir: PathBuf,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

#[test]
fn the_binary_detects_speech_without_a_loaded_model() {
    let data_dir = std::env::temp_dir().join(format!("stt-vad-test-{}", std::process::id()));
    let mut guard = ChildGuard {
        child: Command::new(EXE)
            .args([
                "run",
                "--host",
                "127.0.0.1",
                "--port",
                "54421",
                "--data-dir",
            ])
            .arg(&data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to launch stt-server"),
        data_dir: data_dir.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let info = loop {
        if let Some(info) = discovery::read_server_json(&data_dir) {
            break info;
        }
        assert!(Instant::now() < deadline, "server.json never appeared");
        if let Ok(Some(status)) = guard.child.try_wait() {
            panic!("server process exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let token = std::fs::read_to_string(data_dir.join("auth.token")).unwrap();
    let base = format!("http://127.0.0.1:{}", info.port);

    let (system, vad): (Value, Value) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = reqwest::Client::new();
        let system = client
            .get(format!("{base}/v1/local/system"))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(SPEECH_WAV)
                .file_name("speech.wav")
                .mime_str("audio/wav")
                .unwrap(),
        );
        let response = client
            .post(format!("{base}/v1/audio/vad"))
            .bearer_auth(token.trim())
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        (system, response.json().await.unwrap())
    });

    assert_eq!(
        system["vad"],
        json!({"engine": "silero-v5", "sample_rate": 16000})
    );
    assert!(!vad["segments"].as_array().unwrap().is_empty());
    assert!(vad["speech_ratio"].as_f64().unwrap() > 0.5);
}
