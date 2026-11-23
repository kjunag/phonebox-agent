use std::io::Read;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};
use subtle::ConstantTimeEq;

const LIFETIME_MS: i64 = 5 * 60_000;
const MAX_FAILURES: i32 = 10;

struct Inner {
    code: Option<Vec<u8>>,
    expires_at: i64,
    failures: i32,
    attempt: i64,
}

pub struct Codes {
    inner: Mutex<Inner>,
    claims: AtomicI64,
}

impl Codes {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner { code: None, expires_at: 0, failures: 0, attempt: 0 }),
            claims: AtomicI64::new(0),
        }
    }

    pub fn claims(&self) -> i64 {
        self.claims.load(Ordering::Relaxed)
    }

    /// A new code replaces the previous one. The code is 128 random bits, shown once in the QR link.
    pub fn start(&self, now_ms: i64) -> Option<(String, i64, i64)> {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut bytes).ok()?;
        let code = base64url(&bytes);
        let mut inner = self.inner.lock().ok()?;
        inner.code = Some(code.as_bytes().to_vec());
        inner.expires_at = now_ms + LIFETIME_MS;
        inner.failures = 0;
        inner.attempt += 1;
        Some((code, inner.expires_at, inner.attempt))
    }

    pub fn cancel(&self, attempt: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.attempt == attempt {
                inner.code = None;
            }
        }
    }

    pub fn claim(&self, code: &str, now_ms: i64) -> bool {
        let Ok(mut inner) = self.inner.lock() else { return false };
        let Some(expected) = inner.code.as_deref() else { return false };
        if now_ms > inner.expires_at {
            inner.code = None;
            return false;
        }
        let given = code.as_bytes();
        let matches = given.len() == expected.len() && given.ct_eq(expected).into();
        if !matches {
            inner.failures += 1;
            if inner.failures >= MAX_FAILURES {
                inner.code = None;
            }
            return false;
        }
        inner.code = None;
        self.claims.fetch_add(1, Ordering::Relaxed);
        true
    }
}

impl Default for Codes {
    fn default() -> Self {
        Self::new()
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

pub fn link(pc: &str, fingerprint: &str, port: u16, hosts: &[String], code: &str) -> String {
    let hosts = hosts.iter().map(|host| escape(host)).collect::<Vec<_>>().join(",");
    format!(
        "https://ersinkoc.github.io/phonebox/pair/#v=1&pc={}&fp={}&port={port}&h={hosts}&c={}",
        escape(pc),
        escape(fingerprint),
        escape(code)
    )
}

pub fn check(fingerprint: &str) -> String {
    let text = fingerprint.to_ascii_uppercase();
    if text.len() < 8 {
        text
    } else {
        format!("{} {}", &text[..4], &text[4..8])
    }
}

pub fn svg(text: &str) -> String {
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) else { return String::new() };
    code.render::<svg::Color>()
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .quiet_zone(true)
        .build()
}

/// The code from a claim body. A bad body is a refusal, never a logged error.
pub fn read_code(content_type: &str, body: &[u8]) -> Option<String> {
    if body.len() > 1024 || !json_content_type(content_type) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = value.get("code")?.as_str()?;
    if code.is_empty() || code.len() > 64 { None } else { Some(code.to_string()) }
}

pub fn addresses() -> Vec<String> {
    let Ok(addr) = std::process::Command::new("ip").args(["-4", "-o", "addr", "show"]).output() else { return Vec::new() };
    let route = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    parse_addresses(&String::from_utf8_lossy(&addr.stdout), &route)
}

pub fn macs() -> Vec<String> {
    let mut macs = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else { return macs };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == "lo" { continue }
        if let Ok(mac) = std::fs::read_to_string(entry.path().join("address")) {
            let mac = mac.trim();
            if !mac.is_empty() && mac != "00:00:00:00:00:00" { macs.push(mac.to_string()); }
        }
    }
    macs
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string()
}

pub fn app_download() -> String {
    let path = match std::env::var_os("PHONEBOX_ASSETS") {
        Some(dir) => std::path::PathBuf::from(dir).join("PhoneBox.Agent.csproj"),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/PhoneBox.Agent.csproj"),
    };
    let version = std::fs::read_to_string(path).ok().and_then(|text| {
        text.split("<Version>").nth(1).and_then(|rest| rest.split("</Version>").next()).map(|value| value.trim().to_string())
    }).unwrap_or_else(|| "0.9.29".into());
    format!("https://github.com/ersinkoc/phonebox/releases/download/v{version}/PhoneBox-{version}.apk")
}

fn json_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    if !parts.next().unwrap_or("").trim().eq_ignore_ascii_case("application/json") {
        return false;
    }
    match parts.next() {
        None => true,
        Some(param) => {
            let charset = param.trim().trim_start_matches("charset=").trim().trim_matches('"');
            charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("utf8")
        }
    }
}

fn parse_addresses(addr: &str, route: &str) -> Vec<String> {
    let gateways = default_gateways(route);
    let mut lan = Vec::new();
    let mut tailscale = Vec::new();
    for line in addr.lines() {
        let Some((name, ip)) = parse_addr_line(line) else { continue };
        let Some(octets) = ipv4(&ip) else { continue };
        if octets == [127, 0, 0, 1] { continue }
        let has_gateway = gateways.iter().any(|iface| iface == &name);
        match classify(&name, octets, has_gateway) {
            Some(true) if !tailscale.contains(&ip) => tailscale.push(ip),
            Some(false) if !lan.contains(&ip) => lan.push(ip),
            _ => {}
        }
    }
    lan.extend(tailscale);
    lan
}

fn default_gateways(route: &str) -> Vec<String> {
    route.lines().skip(1).filter_map(|line| {
        let mut fields = line.split_whitespace();
        let iface = fields.next()?.to_string();
        let destination = fields.next()?;
        (destination == "00000000").then_some(iface)
    }).collect()
}

fn parse_addr_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let _index = parts.next()?;
    let name = parts.next()?.to_string();
    let mut previous = "";
    for part in parts {
        if previous == "inet" {
            return Some((name, part.split('/').next()?.to_string()));
        }
        previous = part;
    }
    None
}

fn ipv4(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(octets)
}

/// `Some(true)` is Tailscale, `Some(false)` is a private LAN address, `None` is skipped.
fn classify(name: &str, ip: [u8; 4], has_gateway: bool) -> Option<bool> {
    let tailscale = name.to_ascii_lowercase().contains("tailscale") || (ip[0] == 100 && (64..128).contains(&ip[1]));
    if tailscale { return Some(true) }
    let private = ip[0] == 10 || (ip[0] == 172 && (16..32).contains(&ip[1])) || (ip[0] == 192 && ip[1] == 168);
    if !private || (!has_gateway && virtual_name(name)) { return None }
    Some(false)
}

fn virtual_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["docker", "veth", "virbr", "vmnet", "vbox", "bluetooth", "br-", "cni", "flannel", "podman", "zt", "vethernet", "wsl", "hyper-v"]
        .iter()
        .any(|mark| name.contains(mark))
}

fn escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let n = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8) | bytes[index + 2] as u32;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        out.push(ALPHABET[(n & 63) as usize] as char);
        index += 3;
    }
    if index < bytes.len() {
        let mut n = (bytes[index] as u32) << 16;
        if index + 1 < bytes.len() { n |= (bytes[index + 1] as u32) << 8 }
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if index + 1 < bytes.len() { out.push(ALPHABET[((n >> 6) & 63) as usize] as char); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_keeps_the_code_in_the_fragment() {
        let link = link("Desk PC & co", "ab12", 8766, &["192.168.1.10".into(), "100.64.0.7".into()], "CODE_x-1");
        assert!(link.starts_with("https://ersinkoc.github.io/phonebox/pair/#"));
        assert!(!link.split('#').next().unwrap_or("").contains('?'));
        assert!(link.ends_with("#v=1&pc=Desk%20PC%20%26%20co&fp=ab12&port=8766&h=192.168.1.10,100.64.0.7&c=CODE_x-1"));
        assert!(!link.contains("secret-token"));
    }

    #[test]
    fn the_check_code_is_the_start_of_the_fingerprint() {
        assert_eq!(check(&format!("ab12cd34{}", "0".repeat(56))), "AB12 CD34");
    }

    #[test]
    fn a_code_works_once() {
        let codes = Codes::new();
        let (code, _, _) = codes.start(1_000).unwrap();
        assert!(codes.claim(&code, 1_000));
        assert!(!codes.claim(&code, 1_000));
        assert_eq!(codes.claims(), 1);
    }

    #[test]
    fn an_expired_code_and_too_many_guesses_are_dropped() {
        let codes = Codes::new();
        let (code, expires, attempt) = codes.start(1_000).unwrap();
        assert!(!codes.claim(&code, expires + 1));
        let (code, _, later) = codes.start(2_000).unwrap();
        codes.cancel(attempt);
        assert!(codes.claim(&code, 2_000));
        let (code, _, _) = codes.start(3_000).unwrap();
        codes.cancel(later);
        for i in 0..10 { assert!(!codes.claim(&format!("wrong{i}"), 3_000)); }
        assert!(!codes.claim(&code, 3_000));
    }

    #[test]
    fn a_bad_claim_body_has_no_code() {
        assert_eq!(read_code("text/plain", br#"{"code":"abc"}"#), None);
        assert_eq!(read_code("application/json; charset=nope", br#"{"code":"abc"}"#), None);
        assert_eq!(read_code("application/json", b"not json"), None);
        assert_eq!(read_code("application/json", br#"{"code":"abc"}"#).as_deref(), Some("abc"));
    }

    #[test]
    fn virtual_adapters_without_a_gateway_stay_out() {
        let addr = "\
2: wlp3s0    inet 192.168.1.25/24 brd 192.168.1.255 scope global wlp3s0\n\
3: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\n\
4: tailscale0    inet 100.107.144.19/32 scope global tailscale0\n\
5: lo    inet 127.0.0.1/8 scope host lo\n";
        let route = "Iface Destination Gateway Flags\nwlp3s0 00000000 0101A8C0 0003\n";
        assert_eq!(parse_addresses(addr, route), vec!["192.168.1.25".to_string(), "100.107.144.19".to_string()]);
    }

    #[test]
    fn the_qr_is_an_svg_without_script() {
        let svg = svg("https://ersinkoc.github.io/phonebox/pair/#c=abc");
        assert!(svg.contains("<svg"));
        assert!(!svg.to_ascii_lowercase().contains("<script"));
    }
}
