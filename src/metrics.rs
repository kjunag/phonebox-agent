use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
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
}

struct Inner {
    previous_cpu: Option<(u64, u64)>,
    previous_net: Option<(f64, f64, Instant)>,
    previous_proc: HashMap<i32, u64>,
    previous_proc_at: Option<Instant>,
    sample: Sample,
    groups: Vec<Value>,
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
                },
                groups: Vec::new(),
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
                "gpu": null,
                "netDownBps": sample.net_down_bps,
                "netUpBps": sample.net_up_bps,
                "disks": sample.disks,
                "uptimeSec": sample.uptime_sec,
                "gpus": [],
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
            let Some((name, ticks)) = read_proc(pid) else { continue };
            current.insert(pid, ticks);
            let cpu = self.previous_proc.get(&pid).map(|before| {
                if elapsed <= 0.0 { 0.0 } else { (ticks.saturating_sub(*before) as f64 / 100.0 / elapsed * 100.0).max(0.0) }
            }).unwrap_or(0.0);
            let slot = groups.entry(name.clone()).or_insert((0, 0.0, 0.0, true));
            slot.0 += 1;
            slot.1 += cpu;
            slot.2 += proc_rss_mb(pid);
            if killable(pid, &name) { slot.3 = false; }
        }
        self.previous_proc = current;
        self.previous_proc_at = Some(now);
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap_or(std::cmp::Ordering::Equal));
        groups.into_iter().map(|(name, (count, cpu, mem, locked))| json!({
            "name": name,
            "title": name,
            "count": count,
            "cpu": (cpu * 10.0).round() / 10.0,
            "memMb": (mem * 10.0).round() / 10.0,
            "locked": locked,
            "hasIcon": false,
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
                if !comm.eq_ignore_ascii_case(name) { continue }
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
        json!({"name": name, "title": name, "locked": locked, "instances": instances})
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
            if !comm.eq_ignore_ascii_case(name) { continue }
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
