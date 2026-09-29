//! "Instant startup, model loads in background": run against the real
//! compiled binary. `STT_NEXT_TEST_SLOW_LOAD_MS` (see `app::open_app_at_full`)
//! simulates a huge or CPU-fallback model's slow load without a real GGUF.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use stt_server_next::discovery;

const EXE: &str = env!("CARGO_BIN_EXE_stt-server-next");

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

fn cli(args: &[&str], data_dir: &Path) -> i32 {
    Command::new(EXE)
        .args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run stt-server-next")
        .code()
        .unwrap_or(-1)
}

#[test]
fn stop_and_status_work_while_a_slow_model_load_is_in_progress() {
    let data_dir = std::env::temp_dir().join(format!("stt-slow-load-test-{}", std::process::id()));
    let mut guard = ChildGuard {
        child: Command::new(EXE)
            .args([
                "run",
                "--host",
                "127.0.0.1",
                "--port",
                "54420",
                "--data-dir",
            ])
            .arg(&data_dir)
            // Much longer than the deadlines below, so every check runs mid-load.
            .env("STT_NEXT_TEST_SLOW_LOAD_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to launch stt-server-next"),
        data_dir: data_dir.clone(),
    };

    // The original bug: nothing was discoverable until the load finished.
    let deadline = Instant::now() + Duration::from_secs(15);
    let info = loop {
        if let Some(info) = discovery::read_server_json(&data_dir) {
            break info;
        }
        assert!(
            Instant::now() < deadline,
            "server.json never appeared during the load"
        );
        if let Ok(Some(status)) = guard.child.try_wait() {
            panic!("server process exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(
        cli(&["status"], &data_dir),
        0,
        "status must work while loading"
    );

    let token = std::fs::read_to_string(data_dir.join("auth.token")).unwrap();
    let url = format!("http://127.0.0.1:{}/readiness", info.port);
    let readiness: serde_json::Value = tokio::runtime::Runtime::new().unwrap().block_on(async {
        reqwest::Client::new()
            .get(url)
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    });
    assert_eq!(readiness["status"], "not_ready");
    assert_eq!(readiness["reason"], "loading model");

    let started = Instant::now();
    assert_eq!(cli(&["stop"], &data_dir), 0, "stop must work while loading");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stop took {:?}; it must not wait for the load",
        started.elapsed()
    );
    let _ = guard.child.wait();
}
