//! Optional Windows service: same sync loop, no window, runs without a logged-in user.
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::{dyndns, store, INTERVAL};

const NAME: &str = "DynDNSUpdater";

pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Needs admin. Copies the current user's settings for the service and (re)starts it.
pub fn install() -> Result<(), String> {
    let s = store::load_user();
    if !s.is_complete() {
        return Err("Settings are incomplete".into());
    }
    let _ = uninstall();
    store::save_service(&s)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "DynDNS Updater".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().map_err(|e| e.to_string())?,
        launch_arguments: vec!["--service".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::START).map_err(|e| e.to_string())?;
    service.start::<&str>(&[]).map_err(|e| e.to_string())
}

/// Needs admin. Stops and removes the service and its settings. No service is not an error
/// (the installer's uninstaller always calls this).
pub fn uninstall() -> Result<(), String> {
    if !is_installed() {
        store::delete_service();
        return Ok(());
    }
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
    let result = manager
        .open_service(NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE)
        .and_then(|svc| {
            let _ = svc.stop();
            for _ in 0..20 {
                match svc.query_status() {
                    Ok(st) if st.current_state != ServiceState::Stopped => std::thread::sleep(Duration::from_millis(250)),
                    _ => break,
                }
            }
            svc.delete()
        })
        .map_err(|e| e.to_string());
    store::delete_service();
    result
}

define_windows_service!(ffi_service_main, service_main);

pub fn run() -> Result<(), windows_service::Error> {
    service_dispatcher::start(NAME, ffi_service_main)
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        log(&format!("service error: {e}"));
    }
}

fn run_service() -> Result<(), windows_service::Error> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = service_control_handler::register(NAME, move |c| match c {
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP))?;

    let mut halted = false;
    loop {
        if !halted {
            match store::load_service() {
                Ok(s) => {
                    let st = dyndns::sync(&s, false);
                    log(&format!("{} {}", st.kind, st.detail));
                    // Fatal replies stop updates until settings are saved again (service reinstalled).
                    halted = st.is_fatal();
                }
                Err(e) => log(&e),
            }
        }
        match stop_rx.recv_timeout(INTERVAL) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))
}

fn log(msg: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // shortcut: log grows forever and uses unix seconds, add rotation/local time if it gets used a lot
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(store::service_dir().join("service.log")) {
        let _ = writeln!(f, "{secs} {msg}");
    }
}
