#![windows_subsystem = "windows"]

mod dyndns;
mod service;
mod store;

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, SharedString};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

slint::include_modules!();

pub const INTERVAL: Duration = Duration::from_secs(10 * 60);

thread_local! {
    static TRAY: RefCell<Option<(TrayIcon, [MenuItem; 3])>> = const { RefCell::new(None) };
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--service") => {
            let _ = service::run();
        }
        Some("--install-service") => exit_with(service::install()),
        Some("--uninstall-service") => exit_with(service::uninstall()),
        _ => {
            if already_running() {
                let bs = store::load_user().language == 1;
                return message_box(if bs {
                    "DynDNS Updater već radi. Otvori ga preko ikone u tray-u."
                } else {
                    "DynDNS Updater is already running. Open it from the tray icon."
                });
            }
            if let Err(e) = gui() {
                message_box(&e.to_string());
            }
        }
    }
}

/// Two windows would each send updates, and DynDNS counts repeats as abuse.
fn already_running() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name = wide("Local\\DynDNSUpdater");
    // SAFETY: name is NUL-terminated. The handle is never closed on purpose: it must live as
    // long as this process, and Windows releases it on exit.
    unsafe {
        CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

fn gui() -> Result<(), Box<dyn std::error::Error>> {
    let ui = AppWindow::new()?;
    let s = store::load_user();
    ui.set_username(s.username.into());
    ui.set_password(s.password.into());
    ui.set_hostname(s.hostname.into());
    ui.set_language(s.language);
    ui.set_service_installed(service::is_installed());
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    set_language(s.language);

    ui.on_language_changed({
        let weak = ui.as_weak();
        move |lang| {
            set_language(lang);
            // Remember only the language; unsaved edits in the fields wait for Save.
            let mut saved = store::load_user();
            saved.language = lang;
            let _ = store::save_user(&saved);
            refresh_tray(&weak.unwrap());
        }
    });

    ui.on_save({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            if let Err(e) = store::save_user(&settings(&ui)) {
                return set_status(&ui, "error", &e);
            }
            ui.set_halted(false);
            ui.set_ip("".into()); // new settings: the next check must not trust the old IP
            set_status(&ui, "saved", "");
            // The service keeps its own copy of the settings; refresh it.
            if ui.get_service_installed() {
                elevate_in_background(&ui, "--install-service", |ui, ok| {
                    set_status(ui, if ok { "saved" } else { "service-failed" }, "");
                });
            }
        }
    });

    ui.on_update_now({
        let weak = ui.as_weak();
        move || start_sync(&weak.unwrap(), true)
    });

    ui.on_service_toggled({
        let weak = ui.as_weak();
        move |on| {
            let ui = weak.unwrap();
            let s = settings(&ui);
            if on && !s.is_complete() {
                ui.set_service_installed(false);
                return set_status(&ui, "missing", "");
            }
            if on {
                if let Err(e) = store::save_user(&s) {
                    ui.set_service_installed(false);
                    return set_status(&ui, "error", &e);
                }
            }
            let arg = if on { "--install-service" } else { "--uninstall-service" };
            elevate_in_background(&ui, arg, move |ui, ok| {
                ui.set_service_installed(service::is_installed());
                let kind = match (ok, on) {
                    (false, _) => "service-failed",
                    (true, true) => "service-on",
                    (true, false) => "service-off",
                };
                set_status(ui, kind, "");
            });
        }
    });

    // Auto sync while the window app runs and the service is off.
    let timer = slint::Timer::default();
    let auto = {
        let weak = ui.as_weak();
        move || auto_sync(&weak.unwrap())
    };
    auto();
    timer.start(slint::TimerMode::Repeated, INTERVAL, auto);

    // While the service does the updates, show what it last reported.
    let service_timer = slint::Timer::default();
    let show_service = {
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            if ui.get_service_installed() && !ui.get_busy() {
                show_service_status(&ui);
            }
        }
    };
    show_service();
    service_timer.start(slint::TimerMode::Repeated, Duration::from_secs(30), show_service);

    setup_tray(&ui)?;
    // Closing the window hides it to the tray; "Exit" in the tray menu quits.
    ui.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn settings(ui: &AppWindow) -> store::Settings {
    store::Settings {
        username: ui.get_username().trim().into(),
        password: ui.get_password().into(),
        hostname: ui.get_hostname().trim().into(),
        language: ui.get_language(),
    }
}

fn set_language(lang: i32) {
    let _ = slint::select_bundled_translation(if lang == 1 { "bs" } else { "en" });
}

fn set_status(ui: &AppWindow, kind: &str, detail: &str) {
    ui.set_status_kind(kind.into());
    ui.set_status_detail(detail.into());
    refresh_tray(ui);
}

fn auto_sync(ui: &AppWindow) {
    if !ui.get_service_installed() && !ui.get_halted() && !ui.get_busy() && settings(ui).is_complete() {
        start_sync(ui, false);
    }
}

fn start_sync(ui: &AppWindow, force: bool) {
    let s = settings(ui);
    if !s.is_complete() {
        return set_status(ui, "missing", "");
    }
    ui.set_busy(true);
    set_status(ui, "checking", "");
    let last_ip = ui.get_ip().to_string();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let st = dyndns::sync(&s, force, &last_ip);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            ui.set_halted(st.is_fatal());
            if dyndns::accepted(&st) {
                ui.set_ip(st.detail.as_str().into());
                ui.set_checked_at(now_hhmm().into());
            }
            if st.kind == "offline" {
                let weak = ui.as_weak();
                slint::Timer::single_shot(dyndns::wait_after(&st), move || {
                    if let Some(ui) = weak.upgrade() {
                        auto_sync(&ui);
                    }
                });
            }
            set_status(&ui, &st.kind, &st.detail);
        });
    });
}

fn show_service_status(ui: &AppWindow) {
    let Some((st, time)) = service::read_status() else { return };
    ui.set_halted(st.is_fatal());
    if dyndns::accepted(&st) {
        ui.set_ip(st.detail.as_str().into());
        ui.set_checked_at(time.into());
    }
    set_status(ui, &st.kind, &st.detail);
}

fn setup_tray(ui: &AppWindow) -> Result<(), Box<dyn std::error::Error>> {
    let items = [MenuItem::new("", true, None), MenuItem::new("", true, None), MenuItem::new("", true, None)];
    let menu = Menu::new();
    for item in &items {
        menu.append(item)?;
    }
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(tray_icon::Icon::from_resource(1, None)?)
        .with_tooltip("DynDNS Updater")
        .build()?;
    let ids = [items[0].id().clone(), items[1].id().clone(), items[2].id().clone()];
    TRAY.with(|t| *t.borrow_mut() = Some((tray, items)));
    refresh_tray(ui);

    let weak = ui.as_weak();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let ids = ids.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if e.id == ids[0] {
                let _ = ui.show();
            } else if e.id == ids[1] {
                ui.invoke_update_now();
            } else if e.id == ids[2] {
                let _ = slint::quit_event_loop();
            }
        });
    }));

    let weak = ui.as_weak();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = weak.upgrade_in_event_loop(|ui| {
                let _ = ui.show();
            });
        }
    }));
    Ok(())
}

/// Tray texts live in Rust (the menu is native), so they are translated here.
fn refresh_tray(ui: &AppWindow) {
    let bs = ui.get_language() == 1;
    let labels = if bs { ["Prikaži", "Ažuriraj sada", "Izlaz"] } else { ["Show", "Update now", "Exit"] };
    let status: SharedString = ui.invoke_status_text(ui.get_status_kind(), ui.get_status_detail());
    TRAY.with(|t| {
        if let Some((tray, items)) = &*t.borrow() {
            for (item, label) in items.iter().zip(labels) {
                item.set_text(label);
            }
            let tip = if status.is_empty() { "DynDNS Updater".to_string() } else { format!("DynDNS Updater\n{status}") };
            let _ = tray.set_tooltip(Some(tip));
        }
    });
}

/// Runs `run_elevated` on a worker thread so the window keeps responding while UAC and the
/// service install run; the controls are locked until `done` gets the result.
fn elevate_in_background(ui: &AppWindow, arg: &'static str, done: impl FnOnce(&AppWindow, bool) + Send + 'static) {
    ui.set_busy(true);
    set_status(ui, "service-working", "");
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let ok = run_elevated(arg);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            done(&ui, ok);
        });
    });
}

/// Runs this exe again with admin rights (UAC prompt) and waits. True if it succeeded.
fn run_elevated(arg: &str) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(arg);
    // SAFETY: all pointers outlive the call; the process handle is closed below.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
            return false; // includes the user pressing "No" on the UAC prompt
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        code == 0
    }
}

fn exit_with(r: Result<(), String>) -> ! {
    if let Err(e) = r {
        message_box(&e);
        std::process::exit(1);
    }
    std::process::exit(0)
}

fn message_box(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR};
    let (text, title) = (wide(msg), wide("DynDNS Updater"));
    // SAFETY: both strings are NUL-terminated and live for the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONERROR) };
}

pub fn local_time() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    // SAFETY: GetLocalTime only fills the struct.
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    t
}

fn now_hhmm() -> String {
    let t = local_time();
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
