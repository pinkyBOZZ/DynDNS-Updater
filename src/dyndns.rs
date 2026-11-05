//! DynDNS client: public IP lookup and the dyndns2 update call.
use std::net::ToSocketAddrs;
use std::time::Duration;

use crate::store::Settings;

const USER_AGENT: &str = concat!("Personal - DynDNS Updater - ", env!("CARGO_PKG_VERSION"));

/// Result of one sync, shown in the UI. `kind` is a key the UI translates.
pub struct Status {
    pub kind: String,
    pub detail: String,
}

impl Status {
    fn new(kind: &str, detail: &str) -> Self {
        Status { kind: kind.into(), detail: detail.into() }
    }

    /// DynDNS forbids retrying after these until the user fixes the settings.
    pub fn is_fatal(&self) -> bool {
        !matches!(self.kind.as_str(), "good" | "nochg" | "same" | "error")
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Checks the public IP and sends an update only when it is new.
/// `last_ip` is the last IP DynDNS accepted for these settings ("" if none yet).
/// `force` always sends (the "Update now" button).
pub fn sync(s: &Settings, force: bool, last_ip: &str) -> Status {
    let agent = agent();
    let ip = match public_ip(&agent) {
        Ok(ip) => ip,
        Err(e) => return Status::new("error", &e),
    };
    if !should_send(force, &ip, last_ip, || resolves_to(&s.hostname, &ip)) {
        return Status::new("same", &ip);
    }
    match update(&agent, s, &ip) {
        Ok(body) => parse_response(&body),
        Err(e) => Status::new("error", &e),
    }
}

/// Some ISP resolvers keep old answers past the TTL, so DNS alone would resend the same IP
/// every check, and DynDNS treats repeated `nochg` as abuse.
fn should_send(force: bool, ip: &str, last_ip: &str, dns_matches: impl FnOnce() -> bool) -> bool {
    force || (ip != last_ip && !dns_matches())
}

/// Does this status mean DynDNS now has `detail` as the IP?
pub fn accepted(st: &Status) -> bool {
    matches!(st.kind.as_str(), "good" | "nochg" | "same")
}

fn public_ip(agent: &ureq::Agent) -> Result<String, String> {
    let body = agent
        .get("https://checkip.dyndns.com/")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| e.to_string())?;
    parse_checkip(&body).ok_or_else(|| format!("unexpected checkip reply: {body}"))
}

fn resolves_to(host: &str, ip: &str) -> bool {
    (host, 0)
        .to_socket_addrs()
        .map(|mut addrs| addrs.any(|a| a.ip().to_string() == ip))
        .unwrap_or(false)
}

fn update(agent: &ureq::Agent, s: &Settings, ip: &str) -> Result<String, String> {
    let auth = base64(format!("{}:{}", s.username, s.password).as_bytes());
    agent
        .get("https://members.dyndns.org/v3/update")
        .query("hostname", &s.hostname)
        .query("myip", ip)
        .header("Authorization", format!("Basic {auth}"))
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| e.to_string())
}

/// `<html>...Current IP Address: 1.2.3.4</body></html>` -> `1.2.3.4`
fn parse_checkip(body: &str) -> Option<String> {
    let rest = body.split("Current IP Address:").nth(1)?.trim_start();
    let ip: String = rest.chars().take_while(|c| c.is_ascii_hexdigit() || *c == '.' || *c == ':').collect();
    ip.parse::<std::net::IpAddr>().ok().map(|_| ip)
}

/// `good 1.2.3.4`, `nochg 1.2.3.4`, or an error code like `badauth`.
fn parse_response(body: &str) -> Status {
    let line = body.lines().next().unwrap_or("").trim();
    let (code, rest) = line.split_once(' ').unwrap_or((line, ""));
    match code {
        "good" | "nochg" | "badauth" | "nohost" | "notfqdn" | "abuse" => Status::new(code, rest),
        // Outage or DNS trouble on DynDNS's side: retry later instead of stopping for good.
        "911" | "dnserr" => Status::new("error", line),
        "" => Status::new("error", "empty reply"),
        _ => Status::new("server", line),
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkip() {
        let body = "<html><head><title>Current IP Check</title></head><body>Current IP Address: 93.184.216.34</body></html>";
        assert_eq!(parse_checkip(body).as_deref(), Some("93.184.216.34"));
        assert_eq!(parse_checkip("<html>nope</html>"), None);
    }

    #[test]
    fn responses() {
        let s = parse_response("good 1.2.3.4\n");
        assert_eq!((s.kind.as_str(), s.detail.as_str()), ("good", "1.2.3.4"));
        assert!(!s.is_fatal());
        assert_eq!(parse_response("nochg 1.2.3.4").kind, "nochg");
        assert!(parse_response("badauth").is_fatal());
        assert!(!parse_response("911").is_fatal(), "outage must be retried");
        assert!(!parse_response("dnserr").is_fatal(), "dns error must be retried");
        let s = parse_response("numhost");
        assert_eq!(s.kind, "server");
        assert!(s.is_fatal());
    }

    #[test]
    fn send_decision() {
        assert!(should_send(true, "1.1.1.1", "1.1.1.1", || true), "force always sends");
        assert!(!should_send(false, "1.1.1.1", "1.1.1.1", || false), "stale DNS, IP already accepted");
        assert!(!should_send(false, "2.2.2.2", "1.1.1.1", || true), "DNS already right");
        assert!(should_send(false, "2.2.2.2", "1.1.1.1", || false), "new IP");
        assert!(should_send(false, "2.2.2.2", "", || false), "first run");
    }

    #[test]
    fn base64_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }
}
