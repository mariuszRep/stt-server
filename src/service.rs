#![cfg(windows)]

use std::{
    error::Error,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use std::os::windows::process::CommandExt;

use crate::api::run_http;
use crate::app::{
    data_dir, machine_wide_old_program_dir, machine_wide_program_dir, migrate_machine_wide_data,
    token_file,
};

const NAME: &str = "OpenVibeSttNext";
define_windows_service!(ffi_service_main, service_main);

pub fn dispatch() -> Result<(), Box<dyn Error>> {
    service_dispatcher::start(NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(error) = run_service() {
        let log_path = data_dir().join("logs");
        let _ = fs::create_dir_all(&log_path);
        if let Ok(mut log) = OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_path.join("service.log"))
        {
            let _ = writeln!(log, "Service failure: {error}");
        }
    }
}

fn service_status(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn run_service() -> Result<(), Box<dyn Error>> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let status = service_control_handler::register(NAME, move |event| match event {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    status.set_service_status(service_status(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
    ))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    status.set_service_status(service_status(
        ServiceState::Running,
        ServiceControlAccept::STOP,
    ))?;
    let result = runtime.block_on(run_http(async move {
        let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
    }));
    status.set_service_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    result
}

fn install_dir() -> PathBuf {
    machine_wide_program_dir()
}

/// Best-effort admin check, in the same no-dependency spirit as the
/// `icacls`/`reg` calls already used here: `net session` only succeeds when
/// run elevated. Used to give `service install` a clear refusal instead of a
/// raw "access denied" partway through creating `%ProgramFiles%`/
/// `%ProgramData%` folders.
fn is_elevated() -> bool {
    Command::new("net")
        .args(["session"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn owner_name() -> Result<String, Box<dyn Error>> {
    Ok(format!(
        "{}\\{}",
        std::env::var("USERDOMAIN")?,
        std::env::var("USERNAME")?
    ))
}

fn icacls(path: &Path, rules: &[String]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("icacls").arg(path).args(rules).status()?;
    if !status.success() {
        return Err(format!("Could not protect ACL on {}", path.display()).into());
    }
    Ok(())
}

/// `service install` only makes sense for a machine-wide install (the
/// service always runs as LocalSystem, serving every user of the machine),
/// and setting one up requires writing into `%ProgramFiles%`/`%ProgramData%`,
/// which requires an elevated (Administrator) prompt. Refusing up front, with
/// a clear message, is simpler and less confusing than letting a per-user,
/// non-elevated invocation fail partway through with a raw "access denied"
/// from `fs::create_dir_all`/`icacls`. Choice made here: elevation is used as
/// the proxy for "performing a machine-wide install" rather than re-deriving
/// scope from the current (pre-install) executable location, since a
/// per-user install is never elevated and a machine-wide install always must
/// be, at install time.
fn require_elevated_for_install() -> Result<(), Box<dyn Error>> {
    if is_elevated() {
        Ok(())
    } else {
        Err(
            "service install is only for a machine-wide install: run it from an elevated \
             (Administrator) prompt. A per-user install uses `autostart enable` instead."
                .into(),
        )
    }
}

pub fn install() -> Result<(), Box<dyn Error>> {
    require_elevated_for_install()?;
    let install_dir = install_dir();
    fs::create_dir_all(&install_dir)?;
    let binary = install_dir.join("stt-server-next.exe");
    let source = std::env::current_exe()?;
    if source != binary {
        fs::copy(source, &binary)?;
    }
    // Marks this folder as a machine-wide install for `app::install_scope`,
    // so the binary is still recognized after being moved/copied elsewhere
    // (e.g. by an installer that stages it under a different path first).
    fs::write(install_dir.join(".machine-wide-install"), b"")?;
    // Best effort: the old program folder only ever held a copy of the exe.
    let old_program_dir = machine_wide_old_program_dir();
    if old_program_dir.exists() && old_program_dir != install_dir {
        let _ = fs::remove_dir_all(&old_program_dir);
    }
    let data = migrate_machine_wide_data();
    fs::create_dir_all(&data)?;
    let token_path = data.join("auth.token");
    let owner = owner_name()?;
    icacls(
        &data,
        &[
            "/inheritance:r".to_owned(),
            "/grant:r".to_owned(),
            "SYSTEM:(OI)(CI)F".to_owned(),
            "Administrators:(OI)(CI)F".to_owned(),
            format!("{owner}:RX"),
        ],
    )?;
    token_file(&data)?;
    icacls(
        &token_path,
        &[
            "/inheritance:r".to_owned(),
            "/grant:r".to_owned(),
            "SYSTEM:F".to_owned(),
            "Administrators:F".to_owned(),
            format!("{owner}:R"),
        ],
    )?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from("OpenVibe STT Server Next"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: binary,
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = manager.create_service(
        &info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.set_description("Local GGUF batch transcription and model management")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(15),
            },
            ServiceAction {
                action_type: ServiceActionType::None,
                delay: Duration::ZERO,
            },
        ]),
    })?;
    service.start(&[] as &[OsString])?;
    println!(
        "Installed and started {NAME}; token: {}",
        token_path.display()
    );
    Ok(())
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Builds a hidden `cmd` that removes `dir`, retrying for ~10 s so it still
/// succeeds while the process holding the exe finishes exiting. `raw_arg`
/// keeps the quotes intact: cmd.exe does not understand Rust's \" escaping.
fn delayed_remove_dir(dir: &Path) -> Command {
    let dir = dir.display();
    let script = format!(
        "\"for /l %i in (1,1,10) do @(if exist \"{dir}\" (ping -n 2 127.0.0.1 >nul & rmdir /s /q \"{dir}\"))\""
    );
    let mut command = Command::new("cmd");
    command
        .arg("/C")
        .raw_arg(script)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    command
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        for _ in 0..20 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
    service.delete()?;
    drop(service);
    let dir = install_dir();
    let running_from_install = std::env::current_exe()
        .map(|exe| exe.parent() == Some(dir.as_path()))
        .unwrap_or(false);
    if running_from_install {
        // A running exe cannot delete itself; remove the folder once this process has exited.
        delayed_remove_dir(&dir).spawn()?;
    } else if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    println!("Removed {NAME}; model and state data preserved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_remove_dir_handles_paths_with_spaces() {
        let dir = std::env::temp_dir()
            .join(format!("stt-service-test-{}", std::process::id()))
            .join("STT Server");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stt-server-next.exe"), b"x").unwrap();
        let status = delayed_remove_dir(&dir).status().unwrap();
        assert!(status.success());
        assert!(!dir.exists());
    }

    /// `service install` must refuse with a clear message unless it is
    /// performing a machine-wide install, and (per the choice documented on
    /// `require_elevated_for_install`) that is decided by elevation: a normal,
    /// non-elevated test run is exactly the per-user case, so this asserts a
    /// refusal there while still passing if the suite is ever run elevated.
    #[test]
    fn service_install_refuses_unless_elevated() {
        let result = require_elevated_for_install();
        if is_elevated() {
            assert!(result.is_ok());
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("machine-wide"),
                "expected a machine-wide-install refusal, got: {error}"
            );
        }
    }

    #[test]
    fn install_dir_is_the_unified_machine_wide_program_dir() {
        assert_eq!(install_dir(), machine_wide_program_dir());
    }
}
