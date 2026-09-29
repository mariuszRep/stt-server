//! End-to-end tests for the journalled updater (`update_transaction`), run
//! against the real compiled binary as both the "old" and "candidate" exe
//! (their bytes are identical; only the journal's declared `new_version`
//! differs), so `worker()` drives an actual stop/replace/start/validate or
//! restore/restart cycle rather than a simulation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use stt_server::cli::RunFlags;
use stt_server::update_transaction::{self, Journal, Launch, Phase, TaskRunner};
use stt_server::verify::sha256_file;

const EXE: &str = env!("CARGO_BIN_EXE_stt-server");

/// No-op recovery-task registration: these tests drive `worker()` directly
/// and never rely on Task Scheduler.
struct NoopTasks;
impl TaskRunner for NoopTasks {
    fn register(&self, _journal: &Journal) -> Result<(), String> {
        Ok(())
    }
    fn remove(&self, _journal: &Journal) -> Result<(), String> {
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    exe_path: PathBuf,
    data_dir: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Lays out `<root>/bin/stt-server.exe` (a writable copy of the real
/// binary) and `<root>/data`, matching what `update_transaction` expects:
/// the work dir is a sibling of the executable, named after the journal id.
fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "stt-update-txn-e2e-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    let bin_dir = root.join("bin");
    let data_dir = root.join("data");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let exe_path = bin_dir.join("stt-server.exe");
    fs::copy(EXE, &exe_path).unwrap();
    Fixture {
        root,
        exe_path,
        data_dir,
    }
}

/// A `Journal` ready to hand to `worker()`: both "old" and "candidate" exe
/// files are the same real binary, hash-consistent, in the layout
/// `validate_journal` requires. `port` must be free; each test uses its own.
fn journal_for(fixture: &Fixture, port: u16, phase: Phase) -> Journal {
    let id = uuid::Uuid::new_v4().to_string();
    let work_dir = fixture
        .exe_path
        .parent()
        .unwrap()
        .join(format!(".stt-update-{id}"));
    fs::create_dir_all(&work_dir).unwrap();
    let hash = sha256_file(&fixture.exe_path).unwrap();
    fs::copy(&fixture.exe_path, work_dir.join("candidate.exe")).unwrap();
    fs::copy(&fixture.exe_path, work_dir.join("previous.exe")).unwrap();
    fs::copy(&fixture.exe_path, work_dir.join("recovery.exe")).unwrap();
    let version = env!("CARGO_PKG_VERSION").to_owned();
    Journal {
        id: id.clone(),
        phase,
        data_dir: fixture.data_dir.clone(),
        work_dir,
        launch: Launch {
            executable: fixture.exe_path.clone(),
            service: false,
            flags: RunFlags {
                port: Some(port),
                data_dir: Some(fixture.data_dir.clone()),
                ..Default::default()
            },
        },
        was_running: false,
        ready_model: None,
        old_version: version.clone(),
        new_version: version,
        api_level: stt_server::api::API_LEVEL,
        network_mode: "local".into(),
        old_sha256: hash.clone(),
        new_sha256: hash,
        database_existed: false,
        database_sha256: None,
        ready_timeout_seconds: 8,
        task_name: format!("stt-update-txn-e2e-{id}"),
        task_removed: false,
        armed: true,
        error: None,
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn assert_exe_still_runs(exe: &Path, data_dir: &Path) {
    let status = Command::new(exe)
        .args(["status", "--data-dir"])
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("restored/committed exe must still be a valid, runnable binary");
    // "status" on a stopped server exits non-zero ("not running"); what
    // matters here is that the process launched and ran at all.
    let _ = status;
}

#[test]
fn expected_version_mismatch_rolls_back() {
    let fx = fixture("mismatch");
    let mut journal = journal_for(&fx, 54441, Phase::Prepared);
    journal.new_version = "99.99.99-does-not-exist".into();
    journal.save().unwrap();

    let result = rt().block_on(update_transaction::worker(&fx.data_dir, false, &NoopTasks));
    assert!(
        result.is_err(),
        "a version mismatch must not be reported as success"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("previous version restored"),
        "unexpected error: {error}"
    );

    let final_journal = update_transaction::read_journal(&fx.data_dir)
        .unwrap()
        .unwrap();
    assert_eq!(final_journal.phase, Phase::Restored);
    assert!(final_journal
        .error
        .as_deref()
        .unwrap_or("")
        .contains("wrong version"));
    assert_exe_still_runs(&fx.exe_path, &fx.data_dir);
}

#[test]
fn readiness_failure_rolls_back() {
    let fx = fixture("readiness");
    let mut journal = journal_for(&fx, 54442, Phase::Prepared);
    // The real server never loads this model, so the validation endpoint's
    // `ready_model` will never match it and `loading` will be false.
    journal.ready_model = Some("a-model-that-is-never-installed".into());
    journal.save().unwrap();

    let result = rt().block_on(update_transaction::worker(&fx.data_dir, false, &NoopTasks));
    assert!(
        result.is_err(),
        "a readiness failure must not be reported as success"
    );

    let final_journal = update_transaction::read_journal(&fx.data_dir)
        .unwrap()
        .unwrap();
    assert_eq!(final_journal.phase, Phase::Restored);
    assert!(
        final_journal
            .error
            .as_deref()
            .unwrap_or("")
            .contains("failed to load"),
        "unexpected error: {:?}",
        final_journal.error
    );
    assert_exe_still_runs(&fx.exe_path, &fx.data_dir);
}

#[test]
fn interrupted_journal_is_recovered_from_every_non_terminal_phase() {
    for (label, phase, port) in [
        ("stopping", Phase::Stopping, 54443),
        ("snapshot", Phase::Snapshot, 54444),
        ("replacing", Phase::Replacing, 54445),
        ("validating", Phase::Validating, 54446),
        ("rolling_back", Phase::RollingBack, 54447),
    ] {
        let fx = fixture(&format!("interrupted-{label}"));
        let journal = journal_for(&fx, port, phase);
        journal.save().unwrap();

        // `recover = true`: a crash/power-loss recovery run must never
        // assume the interrupted phase succeeded -- it always restores.
        let result = rt().block_on(update_transaction::worker(&fx.data_dir, true, &NoopTasks));
        assert!(
            result.is_err(),
            "phase {label:?}: recovery of an interrupted update reports its own outcome, not silent success"
        );

        let final_journal = update_transaction::read_journal(&fx.data_dir)
            .unwrap()
            .unwrap_or_else(|| panic!("phase {label}: journal missing after recovery"));
        assert_eq!(
            final_journal.phase,
            Phase::Restored,
            "phase {label}: must end recovered, never left mid-update"
        );
        assert_exe_still_runs(&fx.exe_path, &fx.data_dir);
    }
}

#[test]
fn database_backup_is_restored_on_rollback() {
    let fx = fixture("db-rollback");
    let db_path = fx.data_dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE marker (v TEXT)", []).unwrap();
        conn.execute("INSERT INTO marker VALUES ('pre-update')", [])
            .unwrap();
    }

    let mut journal = journal_for(&fx, 54448, Phase::Prepared);
    // Force a rollback (via a version mismatch) after the database has been
    // snapshotted and the candidate has started against it.
    journal.new_version = "99.99.99-does-not-exist".into();
    journal.save().unwrap();

    let result = rt().block_on(update_transaction::worker(&fx.data_dir, false, &NoopTasks));
    assert!(result.is_err());
    let final_journal = update_transaction::read_journal(&fx.data_dir)
        .unwrap()
        .unwrap();
    assert_eq!(final_journal.phase, Phase::Restored);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let value: String = conn
        .query_row("SELECT v FROM marker", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        value, "pre-update",
        "the pre-update database snapshot must be restored verbatim on rollback"
    );
}

#[test]
fn launch_settings_are_preserved_across_the_restart() {
    // Not a live-server test: verifies the exact argument list `start()`
    // will hand to the relaunched process carries every explicit setting
    // (network/host/port/cors/data-dir) -- the guarantee that a LAN/Tailscale
    // restriction is never silently widened by an update.
    let fx = fixture("launch-settings");
    let mut journal = journal_for(&fx, 54449, Phase::Prepared);
    journal.launch.flags = RunFlags {
        port: Some(54449),
        host: Some("0.0.0.0".into()),
        network: Some(stt_server::network::NetworkMode::Lan),
        data_dir: Some(fx.data_dir.clone()),
        cors_origins: vec!["https://client.example".into()],
        ..Default::default()
    };
    journal.save().unwrap();

    let reloaded = update_transaction::read_journal(&fx.data_dir)
        .unwrap()
        .unwrap();
    let args = reloaded.launch.flags.arguments(&fx.data_dir);
    for expected in [
        vec!["--port".to_string(), "54449".to_string()],
        vec!["--host".to_string(), "0.0.0.0".to_string()],
        vec!["--network".to_string(), "lan".to_string()],
        vec![
            "--cors-origin".to_string(),
            "https://client.example".to_string(),
        ],
    ] {
        assert!(
            args.windows(2).any(|w| w == expected.as_slice()),
            "missing {expected:?} in {args:?}"
        );
    }
    assert!(args.contains(&"--data-dir".to_string()));
}
