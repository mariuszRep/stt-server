//! A real `stt-server` process on a spare port with a temporary data directory, for the tests
//! that need no model: it is killed and its directory removed on drop.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use stt_server::discovery;

const EXE: &str = env!("CARGO_BIN_EXE_stt-server");

pub struct Server {
    child: Child,
    data_dir: PathBuf,
    pub base: String,
    /// The user token (`user.token`), which the model-free routes accept.
    pub token: String,
}

impl Server {
    /// Starts the server on `port` with a data directory named after `name` and waits until it
    /// has written `server.json`.
    pub fn start(name: &str, port: u16) -> Self {
        let data_dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let child = Command::new(EXE)
            .args(["run", "--host", "127.0.0.1", "--port", &port.to_string()])
            .arg("--data-dir")
            .arg(&data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to launch stt-server");
        let mut server = Self {
            child,
            data_dir,
            base: String::new(),
            token: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        let info = loop {
            if let Some(info) = discovery::read_server_json(&server.data_dir) {
                break info;
            }
            assert!(Instant::now() < deadline, "server.json never appeared");
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("server process exited early: {status}");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        server.base = format!("http://127.0.0.1:{}", info.port);
        server.token = std::fs::read_to_string(server.data_dir.join("user.token"))
            .unwrap()
            .trim()
            .to_owned();
        server
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}
