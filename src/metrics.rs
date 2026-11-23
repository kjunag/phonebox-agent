use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Value};

#[derive(Clone)]
struct Sample {
    cpu: Option<f64>,
    cpu_temp: Option<f64>,
    ram_used_gb: f64,
    ram_total_gb: f64,
    net_down_bps: f64,
    net_up_bps: f64,
    uptime_sec: u64,
    disks: Vec<Value>,
    audio: Value,
    media: Value,
    gpus: Vec<Value>,
}

struct Inner {
    previous_cpu: Option<(u64, u64)>,
    previous_net: Option<(f64, f64, Instant)>,
    previous_proc: HashMap<i32, u64>,
    previous_proc_at: Option<Instant>,
    sample: Sample,
    groups: Vec<Value>,
    drm: DrmLoad,
}

pub struct Metrics {
    cpu_name: String,
    host: String,
    inner: Mutex<Inner>,
}

impl Metrics {
    pub fn start() -> std::sync::Arc<Self> {
        let metrics = std::sync::Arc::new(Self {
            cpu_name: read_cpu_name(),
            host: std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string(),
            inner: Mutex::new(Inner {
                previous_cpu: None,
                previous_net: None,
                previous_proc: HashMap::new(),
                previous_proc_at: None,
                sample: Sample {
                    cpu: None,
                    cpu_temp: None,
                    ram_used_gb: 0.0,
                    ram_total_gb: 0.0,
                    net_down_bps: 0.0,
                    net_up_bps: 0.0,
                    uptime_sec: 0,
                    disks: Vec::new(),
                    audio: Value::Null,
                    media: Value::Null,
                    gpus: Vec::new(),
                },
                groups: Vec::new(),
                drm: DrmLoad::default(),
            }),
        });
        let background = std::sync::Arc::clone(&metrics);
        std::thread::spawn(move || {
            loop {
                if let Ok(mut inner) = background.inner.lock() {
                    inner.sample_once();
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
        metrics
    }

    pub fn state(&self, cfg_version: &str) -> Value {
        let Ok(inner) = self.inner.lock() else { return json!({}) };
        let sample = &inner.sample;
        json!({
            "update": null,
            "ts": unix_ms(),
            "host": self.host,
            "os": "linux",
            "cfgVersion": cfg_version,
            "pushVersion": 0,
            "evSeq": 0,
            "events": [],
            "pc": {
                "cpu": sample.cpu,
                "cpuTemp": sample.cpu_temp,
                "cpuName": self.cpu_name,
                "ramUsedGb": sample.ram_used_gb,
                "ramTotalGb": sample.ram_total_gb,
                "gpu": sample.gpus.first().cloned().unwrap_or(Value::Null),
                "netDownBps": sample.net_down_bps,
                "netUpBps": sample.net_up_bps,
                "disks": sample.disks,
                "uptimeSec": sample.uptime_sec,
                "gpus": sample.gpus,
            },
            "media": sample.media,
            "audio": sample.audio,
            "ping": null,
            "rules": [],
            "rulesPaused": null,
            "privacy": { "mic": false, "camera": false, "micApps": [], "cameraApps": [] },
            "obs": null,
            "toggles": {},
            "cycles": null,
        })
    }

    pub fn procs(&self, top: usize) -> Value {
        let Ok(inner) = self.inner.lock() else { return json!({"total": 0, "groupCount": 0, "groups": []}) };
        let total: i64 = inner.groups.iter().map(|group| group["count"].as_i64().unwrap_or(0)).sum();
        json!({
            "total": total,
            "groupCount": inner.groups.len(),
            "groups": inner.groups.iter().take(top).cloned().collect::<Vec<_>>(),
        })
    }
}

impl Inner {
    fn sample_once(&mut self) {
        let (idle, total) = cpu_times();
        let cpu = self.previous_cpu.and_then(|(idle0, total0)| {
            let total_d = total.saturating_sub(total0) as f64;
            let idle_d = idle.saturating_sub(idle0) as f64;
            (total_d > 0.0).then(|| ((100.0 * (total_d - idle_d) / total_d).clamp(0.0, 100.0) * 10.0).round() / 10.0)
        });
        self.previous_cpu = Some((idle, total));
        let (rx, tx) = net_bytes();
        let now = Instant::now();
        let (down, up) = self.previous_net.map(|(rx0, tx0, at)| {
            let seconds = now.duration_since(at).as_secs_f64().max(0.001);
            ((rx - rx0).max(0.0) / seconds, (tx - tx0).max(0.0) / seconds)
        }).unwrap_or((0.0, 0.0));
        self.previous_net = Some((rx, tx, now));
        let (used, total_ram) = mem_gb();
        self.sample = Sample {
            cpu,
            cpu_temp: cpu_temp(),
            ram_used_gb: used,
            ram_total_gb: total_ram,
            net_down_bps: down,
            net_up_bps: up,
            uptime_sec: uptime(),
            disks: disks(),
            audio: audio().unwrap_or(Value::Null),
            media: media().unwrap_or(Value::Null),
            gpus: read_gpus(&mut self.drm),
        };
        self.groups = self.proc_groups();
    }

    fn proc_groups(&mut self) -> Vec<Value> {
        let now = Instant::now();
        let elapsed = self.previous_proc_at.map(|at| now.duration_since(at).as_secs_f64()).unwrap_or(0.0);
        let mut current = HashMap::<i32, u64>::new();
        let mut groups: HashMap<String, (i32, f64, f64, bool)> = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Some((comm, ticks)) = read_proc(pid) else { continue };
            let name = program_key(&comm, exe_name(pid).as_deref());
            current.insert(pid, ticks);
            let cpu = self.previous_proc.get(&pid).map(|before| {
                if elapsed <= 0.0 { 0.0 } else { (ticks.saturating_sub(*before) as f64 / 100.0 / elapsed * 100.0).max(0.0) }
            }).unwrap_or(0.0);
            let slot = groups.entry(name.clone()).or_insert((0, 0.0, 0.0, true));
            slot.0 += 1;
            slot.1 += cpu;
            slot.2 += proc_rss_mb(pid);
            if killable(pid, &comm) { slot.3 = false; }
        }
        self.previous_proc = current;
        self.previous_proc_at = Some(now);
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap_or(std::cmp::Ordering::Equal));
        groups.into_iter().map(|(name, (count, cpu, mem, locked))| json!({
            "name": name,
            "title": program_title(&name),
            "count": count,
            "cpu": (cpu * 10.0).round() / 10.0,
            "memMb": (mem * 10.0).round() / 10.0,
            "locked": locked,
            "hasIcon": has_icon(&name),
        })).collect()
    }
}

impl Metrics {
    pub fn detail(&self, name: &str) -> Value {
        let Ok(inner) = self.inner.lock() else {
            return json!({"name": name, "title": name, "locked": true, "instances": []});
        };
        let elapsed = inner.previous_proc_at.map(|at| Instant::now().duration_since(at).as_secs_f64()).unwrap_or(0.0);
        let mut instances = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
                let Some((comm, ticks, _, start)) = stat_fields(pid) else { continue };
                if !matches_program(pid, name) { continue }
                let cpu = inner.previous_proc.get(&pid).map(|before| {
                    if elapsed <= 0.0 { 0.0 } else { (ticks.saturating_sub(*before) as f64 / 100.0 / elapsed * 100.0).max(0.0) }
                }).unwrap_or(0.0);
                instances.push(json!({
                    "pid": pid,
                    "cpu": (cpu * 10.0).round() / 10.0,
                    "memMb": (proc_rss_mb(pid) * 10.0).round() / 10.0,
                    "startedAt": started_ms(start),
                    "session": null,
                    "window": null,
                    "args": cmdline(pid),
                    "locked": !killable(pid, &comm),
                }));
            }
        }
        instances.sort_by(|a, b| b["cpu"].as_f64().unwrap_or(0.0).partial_cmp(&a["cpu"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
        let locked = protected(name) || instances.iter().all(|item| item["locked"].as_bool().unwrap_or(true));
        json!({"name": name, "title": program_title(name), "locked": locked, "instances": instances})
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

fn read_cpu_name() -> String {
    std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|text| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "model name").then(|| value.trim().to_string())
        })
    }).unwrap_or_else(|| "CPU".into())
}

fn cpu_times() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut parts = text.lines().next().unwrap_or("").split_whitespace().skip(1).map(|part| part.parse::<u64>().unwrap_or(0));
    let user = parts.next().unwrap_or(0);
    let nice = parts.next().unwrap_or(0);
    let system = parts.next().unwrap_or(0);
    let idle = parts.next().unwrap_or(0);
    let iowait = parts.next().unwrap_or(0);
    let irq = parts.next().unwrap_or(0);
    let softirq = parts.next().unwrap_or(0);
    let steal = parts.next().unwrap_or(0);
    let idle = idle + iowait;
    (idle, user + nice + system + idle + irq + softirq + steal)
}

fn mem_gb() -> (f64, f64) {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = 0.0;
    let mut available = 0.0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let kb = parts.next().and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.0) * 1024.0;
        if key == "MemTotal:" { total = kb }
        if key == "MemAvailable:" { available = kb }
    }
    ((((total - available).max(0.0) / GB) * 10.0).round() / 10.0, (total / GB * 10.0).round() / 10.0)
}

fn cpu_temp() -> Option<f64> {
    let milli: f64 = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?.trim().parse().ok()?;
    let celsius = milli / 1000.0;
    (celsius > 0.0 && celsius < 150.0).then_some((celsius * 10.0).round() / 10.0)
}

fn uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime").ok().and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok()).map(|seconds| seconds as u64).unwrap_or(0)
}

fn net_bytes() -> (f64, f64) {
    let text = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let mut rx = 0.0;
    let mut tx = 0.0;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || name.starts_with("docker") || name.starts_with("veth") || name.starts_with("br-") || name.starts_with("virbr") {
            continue;
        }
        let mut cols = rest.split_whitespace();
        rx += cols.next().and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.0);
        tx += cols.nth(7).and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.0);
    }
    (rx, tx)
}

fn disks() -> Vec<Value> {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let Some(text) = run(&["df", "-B1", "-P"]) else { return Vec::new() };
    text.lines().skip(1).filter_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 { return None }
        let mount = cols[5];
        if !(mount == "/" || mount.starts_with("/home") || mount.starts_with("/mnt/") || mount.starts_with("/media/")) { return None }
        let total: f64 = cols[1].parse().ok()?;
        let used: f64 = cols[2].parse().ok()?;
        if total <= 2.0 * GB { return None }
        Some(json!({ "name": mount, "usedGb": (used / GB * 10.0).round() / 10.0, "totalGb": (total / GB * 10.0).round() / 10.0 }))
    }).collect()
}

pub(crate) fn current_audio() -> Value {
    audio().unwrap_or(Value::Null)
}

pub(crate) fn current_media() -> Value {
    media().unwrap_or(Value::Null)
}

fn audio() -> Option<Value> {
    let text = run(&["wpctl", "get-volume", "@DEFAULT_AUDIO_SINK@"])?;
    let level: f64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(json!({ "volume": (level * 100.0).round() as i64, "muted": text.contains("[MUTED]"), "canControl": true }))
}

fn media() -> Option<Value> {
    let text = run(&["playerctl", "metadata", "--format", "{{playerName}}\t{{status}}\t{{title}}\t{{artist}}\t{{album}}\t{{position}}\t{{mpris:length}}"])?;
    let fields: Vec<&str> = text.trim_end_matches('\n').split('\t').collect();
    if fields.len() < 7 { return None }
    let pos_ms = fields[5].parse::<i64>().unwrap_or(0) / 1000;
    let dur_ms = fields[6].parse::<i64>().unwrap_or(0) / 1000;
    Some(json!({
        "app": fields[0].split('.').next().unwrap_or(fields[0]),
        "title": fields[2], "artist": fields[3], "album": fields[4],
        "playing": fields[1] == "Playing", "posMs": pos_ms, "durMs": dur_ms, "artId": 0, "canSeek": dur_ms > 0,
    }))
}

fn program_key(comm: &str, exe: Option<&str>) -> String {
    exe.and_then(|path| {
        let path = path.trim_end_matches(" (deleted)");
        let base = path.rsplit('/').next().unwrap_or(path);
        (!base.is_empty()).then(|| base.to_string())
    }).unwrap_or_else(|| comm.to_string())
}

struct DesktopApp {
    title: String,
    icon: Option<String>,
}

fn program_title(key: &str) -> String {
    desktop_app(key).map(|app| app.title.clone()).unwrap_or_else(|| key.to_string())
}

fn desktop_app(key: &str) -> Option<&'static DesktopApp> {
    let apps = desktop_apps();
    if let Some(app) = apps.get(key) {
        return Some(app);
    }
    apps.iter().find(|(bin, _)| bin.eq_ignore_ascii_case(key)).map(|(_, app)| app)
}

pub fn has_icon(name: &str) -> bool {
    icon_path(name).is_some()
}

pub fn proc_icon(name: &str) -> Option<(Vec<u8>, &'static str)> {
    let path = icon_path(name)?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() || bytes.len() > 1_048_576 { return None }
    let mime = match path.extension().and_then(|ext| ext.to_str()) {
        Some("svg") => "image/svg+xml",
        _ => "image/png",
    };
    Some((bytes, mime))
}

fn icon_path(name: &str) -> Option<PathBuf> {
    if !safe_icon_name(name) { return None }
    let mut cache = ICON_PATHS.lock().ok()?;
    if let Some(found) = cache.get(name) {
        return found.clone();
    }
    let found = find_icon(&icon_id(name));
    cache.insert(name.to_string(), found.clone());
    found
}

fn safe_icon_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != ".."
}

fn icon_id(name: &str) -> String {
    desktop_app(name).and_then(|app| app.icon.clone()).unwrap_or_else(|| name.to_string())
}

fn find_icon(icon: &str) -> Option<PathBuf> {
    if icon.starts_with('/') {
        let path = PathBuf::from(icon);
        return (path.is_file() && is_image(&path)).then_some(path);
    }
    let stem = icon.strip_suffix(".png").or_else(|| icon.strip_suffix(".svg")).unwrap_or(icon);
    if !safe_icon_name(stem) { return None }
    let png = format!("{stem}.png");
    let svg = format!("{stem}.svg");
    for root in data_dirs() {
        for size in ["64x64", "48x48", "128x128", "256x256", "32x32"] {
            for theme in ["hicolor", "Adwaita"] {
                let path = root.join("icons").join(theme).join(size).join("apps").join(&png);
                if path.is_file() { return Some(path) }
            }
        }
        let pixmap = root.join("pixmaps").join(&png);
        if pixmap.is_file() { return Some(pixmap) }
        let scalable = root.join("icons/hicolor/scalable/apps").join(&svg);
        if scalable.is_file() { return Some(scalable) }
    }
    None
}

fn is_image(path: &std::path::Path) -> bool {
    matches!(path.extension().and_then(|ext| ext.to_str()), Some("png" | "svg"))
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(value) = std::env::var_os("XDG_DATA_DIRS") {
        dirs.extend(std::env::split_paths(&value));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    for dir in ["/run/current-system/sw/share", "/usr/local/share", "/usr/share"] {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

static ICON_PATHS: LazyLock<Mutex<HashMap<String, Option<PathBuf>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn matches_program(pid: i32, name: &str) -> bool {
    let Some((comm, _)) = read_proc(pid) else { return false };
    program_key(&comm, exe_name(pid).as_deref()).eq_ignore_ascii_case(name)
}

fn exe_name(pid: i32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(program_key("", Some(path.to_string_lossy().as_ref())))
}

fn desktop_apps() -> &'static HashMap<String, DesktopApp> {
    static CACHE: OnceLock<HashMap<String, DesktopApp>> = OnceLock::new();
    CACHE.get_or_init(load_desktop_apps)
}

fn load_desktop_apps() -> HashMap<String, DesktopApp> {
    let mut apps = HashMap::new();
    for dir in desktop_dirs() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") { continue }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some((bin, app)) = parse_desktop_entry(&text) {
                apps.entry(bin).or_insert(app);
            }
        }
    }
    apps
}

fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(value) = std::env::var_os("XDG_DATA_DIRS") {
        dirs.extend(std::env::split_paths(&value).map(|dir| dir.join("applications")));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    for dir in [
        "/run/current-system/sw/share/applications",
        "/usr/local/share/applications",
        "/usr/share/applications",
        "/var/lib/flatpak/exports/share/applications",
        "/var/lib/snapd/desktop/applications",
    ] {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

fn parse_desktop_entry(text: &str) -> Option<(String, DesktopApp)> {
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            if in_entry { break }
            in_entry = section.trim_end_matches(']') == "Desktop Entry";
            continue;
        }
        if !in_entry { continue }
        if name.is_none() {
            if let Some(value) = line.strip_prefix("Name=") { name = Some(value.to_string()); }
        }
        if exec.is_none() {
            if let Some(value) = line.strip_prefix("Exec=") { exec = Some(value.to_string()); }
        }
        if icon.is_none() {
            if let Some(value) = line.strip_prefix("Icon=") {
                let value = value.trim();
                if !value.is_empty() { icon = Some(value.to_string()); }
            }
        }
    }
    Some((exec_base(exec.as_deref()?)?, DesktopApp { title: name?, icon }))
}

fn exec_base(exec: &str) -> Option<String> {
    let token = exec.trim().trim_matches('"').split_whitespace().next()?.trim_matches('"');
    let base = token.rsplit('/').next().unwrap_or(token);
    (!base.is_empty()).then(|| base.to_string())
}

#[derive(Default)]
struct DrmLoad {
    base: HashMap<(String, String, String), (u64, Instant)>,
    measured: HashSet<String>,
    known: Vec<String>,
    discovered_at: Option<Instant>,
}

struct DrmClient {
    id: String,
    pdev: String,
    busy: Vec<(String, u64)>,
    capacity: HashMap<String, u32>,
}

fn read_gpus(drm: &mut DrmLoad) -> Vec<Value> {
    let mut cards = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        let mut names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
        names.sort();
        for name in names {
            let name = name.to_string_lossy();
            if !is_card_dir(&name) { continue }
            if let Some(card) = read_drm_card(&name) { cards.push(card) }
        }
    }
    let intel: Vec<String> = cards.iter().filter(|card| card.vendor == "intel" && card.util.is_none()).filter_map(|card| card.pdev.clone()).collect();
    if !intel.is_empty() {
        let load = drm.sample(&intel);
        for card in &mut cards {
            if card.vendor == "intel" && card.util.is_none() {
                if let Some(pdev) = &card.pdev {
                    card.util = load.get(pdev).copied();
                }
            }
        }
    }
    let mut gpus: Vec<Value> = cards.into_iter().map(|card| card.json()).chain(nvidia_gpus()).collect();
    gpus.sort_by_key(|gpu| gpu["integrated"].as_bool().unwrap_or(false));
    gpus
}

struct GpuCard {
    vendor: String,
    integrated: bool,
    util: Option<f64>,
    pdev: Option<String>,
    name: String,
    temp: Option<f64>,
    power: Option<f64>,
    power_limit: Option<f64>,
    vram_used: Option<f64>,
    vram_total: Option<f64>,
    fan: Option<f64>,
    clock: Option<f64>,
    hot_spot: Option<f64>,
    mem_temp: Option<f64>,
    mem_clock: Option<f64>,
    fan_rpm: Option<f64>,
    temp_max: Option<f64>,
    source: &'static str,
}

impl GpuCard {
    fn json(self) -> Value {
        json!({
            "name": self.name,
            "util": self.util,
            "temp": self.temp,
            "power": self.power,
            "powerLimit": self.power_limit,
            "vramUsed": self.vram_used,
            "vramTotal": self.vram_total,
            "fan": self.fan,
            "clock": self.clock,
            "vendor": self.vendor,
            "source": self.source,
            "integrated": self.integrated,
            "hotSpot": self.hot_spot,
            "memTemp": self.mem_temp,
            "memClock": self.mem_clock,
            "sharedUsed": Value::Null,
            "sharedTotal": Value::Null,
            "fanRpm": self.fan_rpm,
            "tempMax": self.temp_max,
            "driver": Value::Null,
        })
    }
}

fn is_card_dir(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
}

fn read_drm_card(name: &str) -> Option<GpuCard> {
    let card = format!("/sys/class/drm/{name}");
    let dev = format!("{card}/device");
    let vendor_id = read_hex(&dev, "vendor")?;
    let vendor = match vendor_id {
        0x8086 => "intel",
        0x1002 => "amd",
        _ => return None,
    };
    let device_id = read_hex(&dev, "device").unwrap_or(0);
    let slot = pci_slot(&dev);
    let name = gpu_marketing_name(vendor, vendor_id, device_id, slot.as_deref());
    let hw = std::fs::read_dir(format!("{dev}/hwmon")).ok().and_then(|entries| {
        entries.flatten().map(|entry| entry.path()).find(|path| path.is_dir())
    });
    let temps = hwmon_temps(hw.as_deref());
    let temp = temps.iter().find(|(label, _)| label == "edge" || label == "temp1").map(|(_, value)| *value);
    let power = read_num(hw.as_deref(), "power1_average").or_else(|| read_num(hw.as_deref(), "power1_input")).map(|watts| (watts / 1_000_000.0 * 10.0).round() / 10.0);
    let power_limit = read_num(hw.as_deref(), "power1_cap").filter(|cap| *cap > 0.0).map(|cap| (cap / 1_000_000.0).round());
    let fan_rpm = read_num(hw.as_deref(), "fan1_input");
    let fan = fan_rpm.and_then(|rpm| read_num(hw.as_deref(), "fan1_max").filter(|max| *max > 0.0).map(|max| (100.0 * rpm / max).round()));
    let vram_total = read_num(Some(std::path::Path::new(&dev)), "mem_info_vram_total");
    let vram_used = read_num(Some(std::path::Path::new(&dev)), "mem_info_vram_used");
    let clock = if vendor == "amd" {
        dpm_current(&read_text(&dev, "pp_dpm_sclk").unwrap_or_default())
    } else {
        read_num(Some(std::path::Path::new(&card)), "gt_cur_freq_mhz")
    };
    let mem_clock = if vendor == "amd" { dpm_current(&read_text(&dev, "pp_dpm_mclk").unwrap_or_default()) } else { None };
    let integrated = if vendor == "intel" {
        !name.to_ascii_lowercase().contains("dg2") && !intel_discrete(&name)
    } else {
        vram_total.is_some_and(|bytes| bytes < 4.0 * 1024.0 * 1024.0 * 1024.0) && !amd_discrete(&name)
    };
    Some(GpuCard {
        vendor: vendor.to_string(),
        integrated,
        util: read_num(Some(std::path::Path::new(&dev)), "gpu_busy_percent"),
        pdev: slot,
        name,
        temp,
        power,
        power_limit,
        vram_used: vram_used.map(|bytes| bytes / (1024.0 * 1024.0 * 1024.0)),
        vram_total: vram_total.map(|bytes| bytes / (1024.0 * 1024.0 * 1024.0)),
        fan,
        clock,
        hot_spot: temps.iter().find(|(label, _)| label == "junction").map(|(_, value)| *value),
        mem_temp: temps.iter().find(|(label, _)| label == "mem").map(|(_, value)| *value),
        mem_clock,
        fan_rpm,
        temp_max: read_num(hw.as_deref(), "temp1_crit").filter(|crit| *crit > 0.0).map(|crit| crit / 1000.0),
        source: "sysfs",
    })
}

fn gpu_marketing_name(vendor: &str, vendor_id: u32, device_id: u32, slot: Option<&str>) -> String {
    let raw = slot.and_then(udev_model).or_else(|| pci_model(vendor_id, device_id));
    let mut name = raw.map(|value| bracket_name(&value).unwrap_or(value)).unwrap_or_else(|| if vendor == "amd" { "AMD GPU" } else { "Intel GPU" }.to_string());
    let lower = name.to_ascii_lowercase();
    if vendor == "amd" && !lower.contains("amd") { name = format!("AMD {name}") }
    if vendor == "intel" && !lower.contains("intel") { name = format!("Intel {name}") }
    name
}

fn bracket_name(name: &str) -> Option<String> {
    let start = name.find('[')?;
    let end = name[start + 1..].find(']')?;
    let inner = name[start + 1..start + 1 + end].trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

fn udev_model(slot: &str) -> Option<String> {
    let text = std::fs::read_to_string(format!("/run/udev/data/+pci:{slot}")).ok()?;
    for line in text.lines() {
        if let Some(model) = line.strip_prefix("E:ID_MODEL_FROM_DATABASE=") {
            let model = model.trim();
            if !model.is_empty() { return Some(model.to_string()) }
        }
    }
    None
}

fn pci_model(vendor: u32, device: u32) -> Option<String> {
    static CACHE: OnceLock<HashMap<(u32, u32), String>> = OnceLock::new();
    CACHE.get_or_init(load_pci_names).get(&(vendor, device)).cloned()
}

fn load_pci_names() -> HashMap<(u32, u32), String> {
    let mut names = HashMap::new();
    let Some(text) = pci_ids_text() else { return names };
    let mut vendor = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') { continue }
        if line.starts_with('\t') {
            let Some(vendor) = vendor else { continue };
            let rest = line.trim_start_matches('\t');
            if rest.starts_with('\t') || rest.len() < 6 { continue }
            let Ok(device) = u32::from_str_radix(&rest[..4], 16) else { continue };
            let name = rest[4..].trim();
            if !name.is_empty() { names.entry((vendor, device)).or_insert_with(|| name.to_string()); }
        } else if line.len() >= 6 {
            vendor = u32::from_str_radix(&line[..4], 16).ok();
        }
    }
    names
}

fn pci_ids_text() -> Option<String> {
    for path in [
        "/usr/share/hwdata/pci.ids",
        "/usr/share/misc/pci.ids",
        "/usr/share/pci.ids",
        "/run/current-system/sw/share/hwdata/pci.ids",
        "/run/current-system/sw/share/pci.ids",
    ] {
        if let Ok(text) = std::fs::read_to_string(path) { return Some(text) }
    }
    None
}

fn pci_slot(dev: &str) -> Option<String> {
    let text = read_text(dev, "uevent")?;
    for line in text.lines() {
        if let Some(slot) = line.strip_prefix("PCI_SLOT_NAME=") {
            let slot = slot.trim().to_ascii_lowercase();
            if slot.split(':').count() >= 3 { return Some(slot) }
        }
    }
    None
}

fn hwmon_temps(hw: Option<&std::path::Path>) -> Vec<(String, f64)> {
    let Some(hw) = hw else { return Vec::new() };
    let mut temps = Vec::new();
    for index in 1..=8 {
        let Some(milli) = read_num(Some(hw), &format!("temp{index}_input")) else { continue };
        let label = read_text(hw.to_str().unwrap_or(""), &format!("temp{index}_label")).unwrap_or_else(|| format!("temp{index}")).to_ascii_lowercase();
        temps.push((label, (milli / 1000.0 * 10.0).round() / 10.0));
    }
    temps
}

fn read_text(dir: &str, file: &str) -> Option<String> {
    let text = std::fs::read_to_string(format!("{dir}/{file}")).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn read_num(dir: Option<&std::path::Path>, file: &str) -> Option<f64> {
    let dir = dir?.to_str()?;
    read_text(dir, file)?.parse().ok()
}

fn read_hex(dir: &str, file: &str) -> Option<u32> {
    let text = read_text(dir, file)?;
    let text = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(&text);
    u32::from_str_radix(text, 16).ok()
}

fn dpm_current(text: &str) -> Option<f64> {
    for line in text.lines() {
        let line = line.trim();
        if !line.ends_with('*') { continue }
        let mhz = line.split(':').nth(1)?.trim().trim_end_matches('*').trim();
        let number: String = mhz.chars().take_while(|ch| ch.is_ascii_digit() || *ch == '.').collect();
        if let Ok(value) = number.parse::<f64>() { return Some(value) }
    }
    None
}

fn intel_discrete(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace("(tm)", "").replace('™', "");
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    for (index, part) in parts.iter().enumerate() {
        if *part != "arc" { continue }
        let mut next = index + 1;
        if parts.get(next) == Some(&"pro") { next += 1 }
        let Some(token) = parts.get(next) else { continue };
        let bytes = token.as_bytes();
        if (3..=4).contains(&bytes.len()) && (bytes[0] == b'a' || bytes[0] == b'b') && bytes[1..].iter().all(|ch| ch.is_ascii_digit()) {
            return true;
        }
    }
    false
}

fn amd_discrete(name: &str) -> bool {
    let parts: Vec<&str> = name.split_whitespace().collect();
    for (index, part) in parts.iter().enumerate() {
        if part.eq_ignore_ascii_case("rx") {
            if parts.get(index + 1).is_some_and(|token| { let digits = token.chars().take_while(|ch| ch.is_ascii_digit()).count(); (3..=4).contains(&digits) }) {
                return true;
            }
        }
        let bytes = part.as_bytes();
        if bytes.len() == 5 && (bytes[0] == b'W' || bytes[0] == b'w') && bytes[1..].iter().all(|ch| ch.is_ascii_digit()) {
            return true;
        }
    }
    false
}

impl DrmLoad {
    fn sample(&mut self, pdevs: &[String]) -> HashMap<String, f64> {
        let now = Instant::now();
        if self.discovered_at.is_none_or(|at| now.duration_since(at).as_secs() >= 30) {
            self.known = discover_fdinfo();
            self.discovered_at = Some(now);
        }
        let mut clients = Vec::new();
        self.known.retain(|path| match std::fs::read_to_string(path) {
            Ok(text) => match parse_fdinfo(&text) {
                Some(client) => { clients.push(client); true }
                None => false,
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        });
        self.observe(&dedupe_clients(clients), now, pdevs)
    }

    fn observe(&mut self, clients: &[DrmClient], now: Instant, pdevs: &[String]) -> HashMap<String, f64> {
        let mut shares: HashMap<(String, String), f64> = HashMap::new();
        let mut present = HashSet::new();
        let mut measured_now = HashSet::new();
        for client in clients {
            present.insert(client.pdev.clone());
            if client.busy.is_empty() { continue }
            for (class, busy) in &client.busy {
                let key = (client.pdev.clone(), client.id.clone(), class.clone());
                let Some((before, at)) = self.base.get(&key).copied() else {
                    self.base.insert(key, (*busy, now));
                    continue;
                };
                if *busy < before { continue }
                let elapsed = now.duration_since(at).as_secs_f64();
                if elapsed <= 0.0 {
                    self.base.insert(key, (before, at));
                    continue;
                }
                let engines = client.capacity.get(class).copied().filter(|count| *count > 1).unwrap_or(1) as f64;
                shares.entry((client.pdev.clone(), class.clone())).and_modify(|share| *share += (*busy - before) as f64 / (elapsed * 1e9) / engines).or_insert((*busy - before) as f64 / (elapsed * 1e9) / engines);
                self.base.insert(key, (*busy, now));
                self.measured.insert(client.pdev.clone());
                measured_now.insert(client.pdev.clone());
            }
        }
        let mut load = HashMap::new();
        for pdev in pdevs {
            let idle = self.measured.contains(pdev) && !present.contains(pdev);
            if !measured_now.contains(pdev) && !idle { continue }
            let max = shares.iter().filter(|((card, _), _)| card == pdev).map(|(_, share)| *share).fold(0.0, f64::max);
            let pct = (max * 100.0).clamp(0.0, 100.0);
            load.insert(pdev.clone(), (pct * 10.0).round() / 10.0);
        }
        load
    }
}

fn dedupe_clients(clients: Vec<DrmClient>) -> Vec<DrmClient> {
    let mut by_id: HashMap<(String, String), DrmClient> = HashMap::new();
    for client in clients {
        by_id.entry((client.pdev.clone(), client.id.clone())).and_modify(|kept| {
            if client.busy.len() > kept.busy.len() { *kept = client.clone_busy(); }
        }).or_insert(client);
    }
    by_id.into_values().collect()
}

impl DrmClient {
    fn clone_busy(&self) -> Self {
        Self { id: self.id.clone(), pdev: self.pdev.clone(), busy: self.busy.clone(), capacity: self.capacity.clone() }
    }
}

fn discover_fdinfo() -> Vec<String> {
    let mut paths = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return paths };
    let mut pids: Vec<_> = entries.flatten().filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok()).collect();
    pids.sort_unstable();
    let mut fds = 0;
    for pid in pids {
        let Ok(files) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for file in files.flatten() {
            fds += 1;
            if fds > 200_000 || paths.len() >= 4096 { return paths }
            let Ok(target) = std::fs::read_link(file.path()) else { continue };
            if !target.to_string_lossy().starts_with("/dev/dri/") { continue }
            paths.push(format!("/proc/{pid}/fdinfo/{}", file.file_name().to_string_lossy()));
        }
    }
    paths
}

fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let mut driver = None;
    let mut id = None;
    let mut pdev = None;
    let mut engine = Vec::new();
    let mut cycles = Vec::new();
    let mut totals = HashMap::new();
    let mut capacity = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        if !key.starts_with("drm-") || value.is_empty() { continue }
        match key {
            "drm-driver" => driver = Some(value.to_string()),
            "drm-client-id" => id = Some(value.to_string()),
            "drm-pdev" => pdev = Some(value.to_ascii_lowercase()),
            _ if let Some(class) = key.strip_prefix("drm-engine-capacity-") => {
                if let Ok(count) = value.parse() { capacity.insert(class.to_string(), count); }
            }
            _ if let Some(class) = key.strip_prefix("drm-engine-") => {
                if let Some(ns) = drm_counter(value, Some("ns")) { engine.push((class.to_string(), ns)); }
            }
            _ if let Some(class) = key.strip_prefix("drm-total-cycles-") => {
                if let Some(total) = drm_counter(value, None) { totals.insert(class.to_string(), total); }
            }
            _ if let Some(class) = key.strip_prefix("drm-cycles-") => {
                if let Some(count) = drm_counter(value, None) { cycles.push((class.to_string(), count)); }
            }
            _ => {}
        }
    }
    let driver = driver?;
    if driver != "i915" && driver != "xe" { return None }
    let mut busy = Vec::new();
    for (class, count) in cycles {
        if let Some(total) = totals.get(&class) {
            busy.push((class, count));
            let _ = total;
        }
    }
    for (class, ns) in engine {
        if !busy.iter().any(|(name, _)| name == &class) { busy.push((class, ns)); }
    }
    Some(DrmClient { id: id?, pdev: pdev?, busy, capacity })
}

fn drm_counter(value: &str, unit: Option<&str>) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number = parts.next()?;
    let rest = parts.next();
    if rest != unit { return None }
    number.parse().ok()
}

fn nvidia_gpus() -> Vec<Value> {
    let Ok(names) = Command::new("nvidia-smi").args(["--query-gpu=index,name,driver_version", "--format=csv,noheader"]).output() else { return Vec::new() };
    let Ok(stats) = Command::new("nvidia-smi").args(["--query-gpu=index,utilization.gpu,temperature.gpu,power.draw,power.limit,memory.used,memory.total,fan.speed,clocks.gr,clocks.mem", "--format=csv,noheader,nounits"]).output() else { return Vec::new() };
    if !names.status.success() || !stats.status.success() { return Vec::new() }
    let names = parse_nvidia_names(&String::from_utf8_lossy(&names.stdout));
    String::from_utf8_lossy(&stats.stdout).lines().filter_map(|line| nvidia_line(line, &names)).collect()
}

fn parse_nvidia_names(text: &str) -> HashMap<i64, (String, Option<String>)> {
    let mut names = HashMap::new();
    for line in text.lines() {
        let Some((index, rest)) = line.split_once(',') else { continue };
        let Ok(index) = index.trim().parse::<i64>() else { continue };
        let (name, driver) = rest.rsplit_once(',').map(|(name, driver)| (name.trim(), driver.trim())).unwrap_or((rest.trim(), ""));
        if name.is_empty() { continue }
        let driver = (driver != "[N/A]" && !driver.is_empty()).then(|| driver.to_string());
        names.insert(index, (name.to_string(), driver));
    }
    names
}

fn nvidia_line(line: &str, names: &HashMap<i64, (String, Option<String>)>) -> Option<Value> {
    let fields: Vec<Option<f64>> = line.split(',').map(|field| field.trim().parse().ok()).collect();
    if fields.len() < 9 { return None }
    let index = fields[0]?;
    if index < 0.0 { return None }
    let (name, driver) = names.get(&(index as i64)).cloned().unwrap_or_else(|| ("NVIDIA GPU".to_string(), None));
    Some(json!({
        "name": name,
        "util": fields[1],
        "temp": fields[2],
        "power": fields[3],
        "powerLimit": fields[4],
        "vramUsed": fields[5].map(|mib| mib / 1024.0),
        "vramTotal": fields[6].map(|mib| mib / 1024.0),
        "fan": fields[7],
        "clock": fields[8],
        "vendor": "nvidia",
        "source": "nvidia-smi",
        "integrated": false,
        "hotSpot": Value::Null,
        "memTemp": Value::Null,
        "memClock": fields.get(9).copied().unwrap_or(None),
        "sharedUsed": Value::Null,
        "sharedTotal": Value::Null,
        "fanRpm": Value::Null,
        "tempMax": Value::Null,
        "driver": driver,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firefox_content_processes_use_the_firefox_binary() {
        let key = program_key("Isolated Web Co", Some("/nix/store/firefox/lib/firefox/firefox"));
        assert_eq!(key, "firefox");
        assert_eq!(program_key(".firefox-wrappe", Some("/nix/store/firefox/bin/firefox")), "firefox");
    }

    #[test]
    fn a_desktop_file_names_firefox_and_ignores_later_actions() {
        let text = "[Desktop Entry]\nExec=firefox --name firefox %U\nIcon=firefox\nName=Firefox\n\n[Desktop Action new-window]\nName=New Window\nIcon=other\n";
        let (bin, app) = parse_desktop_entry(text).unwrap();
        assert_eq!(bin, "firefox");
        assert_eq!(app.title, "Firefox");
        assert_eq!(app.icon.as_deref(), Some("firefox"));
    }

    #[test]
    fn an_icon_name_cannot_escape_its_directory() {
        assert!(!safe_icon_name("../firefox"));
        assert!(!safe_icon_name("/usr/share/icons/firefox.png"));
        assert!(safe_icon_name("firefox"));
    }

    #[test]
    fn meteor_lake_is_named_from_the_bracket_and_counts_as_integrated() {
        assert_eq!(bracket_name("Meteor Lake-P [Intel Arc Graphics]").as_deref(), Some("Intel Arc Graphics"));
        assert!(!intel_discrete("Intel Arc Graphics"));
        assert!(intel_discrete("Intel Arc A770"));
        assert!(intel_discrete("Arc(TM) Pro A40"));
        assert!(!amd_discrete("Radeon 780M Graphics"));
        assert!(amd_discrete("Radeon RX 7900 XTX"));
        assert_eq!(dpm_current("0: 500Mhz\n1: 2482Mhz *\n"), Some(2482.0));
    }

    #[test]
    fn intel_load_is_the_engine_time_between_two_samples() {
        let text = "drm-driver:\ti915\ndrm-client-id:\t3\ndrm-pdev:\t0000:00:02.0\ndrm-engine-render:\t0 ns\ndrm-engine-capacity-video:\t2\n";
        let mut client = parse_fdinfo(text).unwrap();
        assert_eq!(client.pdev, "0000:00:02.0");
        assert_eq!(client.capacity.get("video"), Some(&2));
        let mut load = DrmLoad::default();
        let pdevs = vec!["0000:00:02.0".to_string()];
        let start = Instant::now() - std::time::Duration::from_secs(1);
        assert!(load.observe(std::slice::from_ref(&client), start, &pdevs).is_empty());
        client.busy[0].1 = 1_000_000_000;
        let now = start + std::time::Duration::from_secs(1);
        assert_eq!(load.observe(std::slice::from_ref(&client), now, &pdevs).get("0000:00:02.0"), Some(&100.0));
    }

    #[test]
    fn an_nvidia_line_keeps_memory_in_gigabytes() {
        let mut names = HashMap::new();
        names.insert(0, ("NVIDIA GeForce RTX 4070".to_string(), Some("560.35".to_string())));
        let gpu = nvidia_line("0, 12, 48, 30, 200, 1024, 12288, 40, 1800, 9000", &names).unwrap();
        assert_eq!(gpu["name"], "NVIDIA GeForce RTX 4070");
        assert_eq!(gpu["vramUsed"], 1.0);
        assert_eq!(gpu["vramTotal"], 12.0);
        assert_eq!(gpu["memClock"], 9000.0);
        assert_eq!(gpu["vendor"], "nvidia");
    }
}

fn read_proc(pid: i32) -> Option<(String, u64)> {
    let (name, ticks, _, _) = stat_fields(pid)?;
    Some((name, ticks))
}

fn stat_fields(pid: i32) -> Option<(String, u64, i32, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 2..].split_whitespace().collect();
    let ppid = rest.get(1)?.parse().ok()?;
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    let start: u64 = rest.get(19)?.parse().ok()?;
    Some((name, utime + stime, ppid, start))
}

pub struct KillOutcome {
    pub killed: i32,
    pub failed: i32,
    pub error: Option<String>,
}

pub fn kill_named(name: &str) -> KillOutcome {
    if name.trim().is_empty() { return outcome(0, 0, Some("no name given")) }
    if protected(name) { return outcome(0, 0, Some(&format!("{name} is protected and cannot be terminated"))) }
    let mut killed = 0;
    let mut failed = 0;
    let mut seen = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Some((comm, _)) = read_proc(pid) else { continue };
            if !matches_program(pid, name) { continue }
            seen += 1;
            if !killable(pid, &comm) { failed += 1; continue }
            if term(pid) { killed += 1 } else { failed += 1 }
        }
    }
    if seen == 0 { return outcome(0, 0, Some("process not found")) }
    if killed == 0 { return outcome(0, failed, Some("access denied (it may be running as administrator)")) }
    outcome(killed, failed, None)
}

pub fn kill_pid(pid: i32) -> KillOutcome {
    if pid == 0 { return outcome(0, 0, Some("PID 0 is protected and cannot be terminated")) }
    if pid == std::process::id() as i32 { return outcome(0, 0, Some("that is the PhoneBox agent itself")) }
    let Some((name, _, _, start)) = stat_fields(pid) else { return outcome(0, 0, Some("process not found")) };
    if protected(&name) { return outcome(0, 0, Some(&format!("{name} is protected and cannot be terminated"))) }
    if !mine(pid) { return outcome(0, 0, Some("process belongs to another session or user")) }
    let children = descendants(pid, start);
    if !term(pid) { return outcome(0, 1, Some("could not terminate")) }
    let mut killed = 1;
    let mut skipped = 0;
    for child in children {
        let Some((child_name, _, _, _)) = stat_fields(child) else { continue };
        if !killable(child, &child_name) { skipped += 1; continue }
        if term(child) { killed += 1 } else { skipped += 1 }
    }
    outcome(killed, skipped, None)
}

fn descendants(root: i32, root_start: u64) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<(i32, u64)>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Some((_, _, ppid, start)) = stat_fields(pid) else { continue };
            children.entry(ppid).or_default().push((pid, start));
        }
    }
    let mut out = Vec::new();
    let mut queue = vec![(root, root_start)];
    let mut seen = std::collections::HashSet::from([root]);
    while let Some((parent, parent_start)) = queue.pop() {
        let Some(kids) = children.get(&parent) else { continue };
        for &(pid, start) in kids {
            if start >= parent_start && seen.insert(pid) {
                out.push(pid);
                queue.push((pid, start));
            }
        }
    }
    out
}

fn killable(pid: i32, name: &str) -> bool {
    pid > 0 && pid != std::process::id() as i32 && !protected(name) && mine(pid)
}

fn protected(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "systemd", "init", "kthreadd", "Xorg", "Xwayland", "gnome-shell", "gnome-session-binary", "plasmashell",
        "kwin_x11", "kwin_wayland", "mutter", "niri", "sway", "Hyprland", "river", "labwc", "wayfire", "weston",
        "cosmic-comp", "xfwm4", "marco", "muffin", "cinnamon", "openbox", "pipewire", "pipewire-pulse", "wireplumber", "pulseaudio",
        "dbus-daemon", "dbus-broker", "sshd", "login", "gdm", "sddm", "lightdm", "NetworkManager", "polkitd",
        "udisksd", "upowerd", "PhoneBoxAgent", "phoneboxagent", "adb", "dotnet",
    ];
    NAMES.iter().any(|item| item.eq_ignore_ascii_case(name))
}

fn mine(pid: i32) -> bool {
    uid_of(pid) == uid_of(std::process::id() as i32)
}

fn uid_of(pid: i32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = text.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn term(pid: i32) -> bool {
    unsafe { kill(pid, 15) == 0 }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn outcome(killed: i32, failed: i32, error: Option<&str>) -> KillOutcome {
    KillOutcome { killed, failed, error: error.map(str::to_string) }
}

fn cmdline(pid: i32) -> Value {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return Value::Null };
    let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
    let text = text.trim();
    if text.is_empty() { Value::Null } else { Value::String(text.chars().take(180).collect()) }
}

fn started_ms(start_ticks: u64) -> Option<i64> {
    let uptime: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    let now = unix_ms();
    let boot = now - (uptime * 1000.0) as i64;
    Some(boot + (start_ticks as f64 / 100.0 * 1000.0) as i64)
}

fn proc_rss_mb(pid: i32) -> f64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { return 0.0 };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0.0);
            return kb / 1024.0;
        }
    }
    0.0
}

fn run(args: &[&str]) -> Option<String> {
    let output = Command::new("timeout").args(["2"]).args(args).output().ok()?;
    if output.stdout.is_empty() { return None }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn set_volume(level: i32) -> bool {
    let level = level.clamp(0, 100);
    let fraction = format!("{:.2}", level as f64 / 100.0);
    let ok = Command::new("timeout").args(["2", "wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", &fraction]).status().map(|s| s.success()).unwrap_or(false);
    if level > 0 {
        let _ = Command::new("timeout").args(["2", "wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", "0"]).status();
    }
    ok
}

pub fn step_volume(delta: i32) -> bool {
    let Some(current) = audio() else { return false };
    set_volume(current["volume"].as_i64().unwrap_or(0) as i32 + delta)
}

pub fn toggle_mute() -> bool {
    Command::new("timeout").args(["2", "wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).status().map(|s| s.success()).unwrap_or(false)
}

pub fn media_action(action: &str, ms: Option<i64>) -> bool {
    let args: Vec<String> = match action {
        "playpause" => vec!["play-pause".into()],
        "next" => vec!["next".into()],
        "prev" => vec!["previous".into()],
        "seek" => {
            let Some(ms) = ms else { return false };
            vec!["position".into(), format!("{:.3}", ms as f64 / 1000.0)]
        }
        _ => return false,
    };
    let mut command = vec!["playerctl".to_string()];
    command.extend(args);
    let refs: Vec<&str> = command.iter().map(String::as_str).collect();
    Command::new("timeout").args(["2"]).args(&refs).status().map(|s| s.success()).unwrap_or(false)
}
