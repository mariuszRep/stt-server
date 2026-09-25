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

use crate::api::run_http;
use crate::app::{data_dir, token_file};

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
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("OpenVibeAI")
        .join("STT Server Next")
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

pub fn install() -> Result<(), Box<dyn Error>> {
    let install_dir = install_dir();
    fs::create_dir_all(&install_dir)?;
    let binary = install_dir.join("stt-server-next.exe");
    let source = std::env::current_exe()?;
    if source != binary {
        fs::copy(source, &binary)?;
    }
    let data = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("OpenVibeAI")
        .join("STT Server Next");
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
        launch_arguments: vec![OsString::from("service")],
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
    let binary = install_dir().join("stt-server-next.exe");
    if binary.exists() {
        fs::remove_file(binary)?;
    }
    println!("Removed {NAME}; model and state data preserved");
    Ok(())
}
