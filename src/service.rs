//! Optional Windows service: same sync loop, no window, runs without a logged-in user.
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
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
    let access = ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG;
    // Already installed: update it in place. Deleting and recreating fails while anything
    // (a slow sync, the Services window) still holds it, and leaves no service behind.
    let service = match manager.open_service(NAME, access) {
        Ok(svc) => {
            stop_and_wait(&svc);
            svc.change_config(&info).map_err(|e| e.to_string())?; // exe may have moved
            svc
        }
        Err(_) => manager.create_service(&info, access).map_err(|e| e.to_string())?,
    };
    store::save_service(&s)?;
    service.start::<&str>(&[]).map_err(|e| e.to_string())
}

/// A sync can block for up to two 20 s HTTP timeouts before the service sees the stop.
fn stop_and_wait(svc: &windows_service::service::Service) {
    let _ = svc.stop();
    for _ in 0..240 {
        match svc.query_status() {
            Ok(st) if st.current_state != ServiceState::Stopped => std::thread::sleep(Duration::from_millis(250)),
            _ => break,
        }
    }
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
            stop_and_wait(&svc);
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
    let mut last_ip = String::new();
    loop {
        let mut wait = INTERVAL;
        if !halted {
            match store::load_service() {
                Ok(s) => {
                    let st = dyndns::sync(&s, false, &last_ip);
                    log(&format!("{} {}", st.kind, st.detail));
                    write_status(&st);
                    if dyndns::accepted(&st) {
                        last_ip = st.detail.clone();
                    }
                    // Fatal replies stop updates until settings are saved again (service reinstalled).
                    halted = st.is_fatal();
                    wait = dyndns::wait_after(&st);
                }
                Err(e) => {
                    log(&e);
                    write_status(&dyndns::Status { kind: "error".into(), detail: e });
                }
            }
        }
        match stop_rx.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))
}

fn status_file() -> std::path::PathBuf {
    store::service_dir().join("status.txt")
}

/// The window can't read the log (admins only), so the last result goes to a small file
/// without secrets that every local user may read.
fn write_status(st: &dyndns::Status) {
    let path = status_file();
    let new = !path.exists();
    let t = crate::local_time();
    let text = format!("kind={}\ndetail={}\ntime={:02}:{:02}\n", st.kind, st.detail, t.wHour, t.wMinute);
    if fs::write(&path, text).is_ok() && new {
        // Rewrites keep this ACL, so it is set once. S-1-5-32-545 = Users.
        let _ = std::process::Command::new("icacls").arg(&path).args(["/grant", "*S-1-5-32-545:R"]).output();
    }
}

/// Last result written by the service, with the time it was checked.
pub fn read_status() -> Option<(dyndns::Status, String)> {
    parse_status(&fs::read_to_string(status_file()).ok()?)
}

fn parse_status(text: &str) -> Option<(dyndns::Status, String)> {
    let mut st = dyndns::Status { kind: String::new(), detail: String::new() };
    let mut time = String::new();
    for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
        match k {
            "kind" => st.kind = v.into(),
            "detail" => st.detail = v.into(),
            "time" => time = v.into(),
            _ => {}
        }
    }
    (!st.kind.is_empty()).then_some((st, time))
}

const MAX_LOG: u64 = 1024 * 1024;

fn log(msg: &str) {
    let path = store::service_dir().join("service.log");
    drop_if_over(&path, MAX_LOG);
    let t = crate::local_time();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{:04}-{:02}-{:02} {:02}:{:02} {msg}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute);
    }
}

/// Deletes the log once it passes `max` bytes; a fresh one starts with the next line.
fn drop_if_over(path: &Path, max: u64) {
    if fs::metadata(path).map(|m| m.len() > max).unwrap_or(false) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        let (st, time) = parse_status("kind=server\ndetail=numhost x=y\ntime=14:32\n").unwrap();
        assert_eq!((st.kind.as_str(), st.detail.as_str(), time.as_str()), ("server", "numhost x=y", "14:32"));
        assert!(parse_status("").is_none(), "empty file means no status yet");
    }

    #[test]
    fn log_dropped_over_limit() {
        let path = std::env::temp_dir().join(format!("dyndns-log-test-{}.log", std::process::id()));
        fs::write(&path, vec![b'x'; 10]).unwrap();
        drop_if_over(&path, 10);
        assert!(path.exists(), "at the limit stays");
        fs::write(&path, vec![b'x'; 11]).unwrap();
        drop_if_over(&path, 10);
        assert!(!path.exists(), "over the limit is deleted");
        drop_if_over(&path, 10); // missing file is fine
    }
}
