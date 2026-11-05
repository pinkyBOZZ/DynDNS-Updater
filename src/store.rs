//! Settings on disk. Simple `key=value` lines; the password is DPAPI-encrypted.
//! User settings: %APPDATA%\DynDNS\config.txt (user-scope DPAPI).
//! Service settings: %ProgramData%\DynDNS\service.txt (machine-scope DPAPI, SYSTEM + Admins only).
use std::path::PathBuf;
use std::{env, fs, ptr};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};

#[derive(Clone, Default)]
pub struct Settings {
    pub username: String,
    pub password: String,
    pub hostname: String,
    pub language: i32, // 0 = English, 1 = Bosanski
}

impl Settings {
    pub fn is_complete(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty() && !self.hostname.is_empty()
    }
}

fn user_file() -> PathBuf {
    PathBuf::from(env::var("APPDATA").unwrap_or_default()).join("DynDNS").join("config.txt")
}

pub fn service_dir() -> PathBuf {
    PathBuf::from(env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into())).join("DynDNS")
}

fn service_file() -> PathBuf {
    service_dir().join("service.txt")
}

pub fn load_user() -> Settings {
    read(&user_file(), false).unwrap_or_default()
}

pub fn save_user(s: &Settings) -> Result<(), String> {
    write(&user_file(), s, false)
}

pub fn load_service() -> Result<Settings, String> {
    read(&service_file(), true)
}

/// Needs admin. Locks the folder to SYSTEM and Administrators before writing the password.
pub fn save_service(s: &Settings) -> Result<(), String> {
    let dir = service_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Well-known SIDs, so it works on any Windows display language.
    let ok = std::process::Command::new("icacls")
        .arg(&dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return Err("icacls failed to lock the service folder".into());
    }
    write(&service_file(), s, true)
}

pub fn delete_service() {
    let _ = fs::remove_dir_all(service_dir());
}

fn read(path: &PathBuf, machine: bool) -> Result<Settings, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut s = parse(&text);
    if !s.password.is_empty() {
        s.password = String::from_utf8(dpapi(&unhex(&s.password)?, false, machine)?).map_err(|e| e.to_string())?;
    }
    Ok(s)
}

fn write(path: &PathBuf, s: &Settings, machine: bool) -> Result<(), String> {
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut enc = s.clone();
    enc.password = hex(&dpapi(s.password.as_bytes(), true, machine)?);
    fs::write(path, format(&enc)).map_err(|e| e.to_string())
}

fn format(s: &Settings) -> String {
    format!(
        "provider=dyndns\nusername={}\nhostname={}\nlanguage={}\npassword={}\n",
        s.username, s.hostname, s.language, s.password
    )
}

fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
        match k {
            "username" => s.username = v.into(),
            "hostname" => s.hostname = v.into(),
            "language" => s.language = v.parse().unwrap_or(0),
            "password" => s.password = v.into(),
            _ => {}
        }
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or("bad password data".to_string()))
        .collect()
}

/// Encrypts or decrypts with Windows DPAPI. `machine` = any process on this PC may decrypt
/// (needed for the service, which runs as SYSTEM); otherwise only this Windows user can.
fn dpapi(data: &[u8], encrypt: bool, machine: bool) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    let flags = CRYPTPROTECT_UI_FORBIDDEN | if machine { CRYPTPROTECT_LOCAL_MACHINE } else { 0 };
    // SAFETY: input points to live data; out is allocated by Windows and freed below.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(&input, ptr::null(), ptr::null(), ptr::null(), ptr::null(), flags, &mut out)
        } else {
            CryptUnprotectData(&input, ptr::null_mut(), ptr::null(), ptr::null(), ptr::null(), flags, &mut out)
        };
        if ok == 0 {
            return Err(format!("DPAPI failed: {}", std::io::Error::last_os_error()));
        }
        let result = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings { username: "a=b".into(), password: "p@ss=1".into(), hostname: "h.dyndns.org".into(), language: 1 };
        let mut enc = s.clone();
        enc.password = hex(&dpapi(s.password.as_bytes(), true, false).unwrap());
        let back = parse(&format(&enc));
        assert_eq!(back.username, "a=b");
        assert_eq!(back.hostname, "h.dyndns.org");
        assert_eq!(back.language, 1);
        assert_ne!(back.password, "p@ss=1");
        let plain = dpapi(&unhex(&back.password).unwrap(), false, false).unwrap();
        assert_eq!(plain, b"p@ss=1");
    }
}
