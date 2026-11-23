use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub struct Quotas {
    providers: Mutex<Value>,
}

impl Quotas {
    pub fn start() -> Arc<Self> {
        let quotas = Arc::new(Self { providers: Mutex::new(json!([])) });
        let background = Arc::clone(&quotas);
        std::thread::spawn(move || {
            loop {
                let next = background.refresh();
                if let Ok(mut slot) = background.providers.lock() {
                    *slot = next;
                }
                std::thread::sleep(Duration::from_secs(180));
            }
        });
        quotas
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "providers": self.providers.lock().map(|slot| slot.clone()).unwrap_or_else(|_| json!([])),
            "overview": null,
        })
    }

    fn refresh(&self) -> Value {
        let mut providers = Vec::new();
        if let Some(provider) = claude_provider() { providers.push(provider); }
        if let Some(provider) = codex_provider() { providers.push(provider); }
        Value::Array(providers)
    }
}

fn claude_provider() -> Option<Value> {
    let path = home().join(".claude/.credentials.json");
    let text = std::fs::read_to_string(path).ok()?;
    let creds: Value = serde_json::from_str(&text).ok()?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"].as_str().unwrap_or("");
    if token.is_empty() {
        return Some(provider("Claude", &plan_label(oauth), &[], None, "Not signed in to Claude Code"));
    }
    let expires = oauth["expiresAt"].as_i64().unwrap_or(0);
    let now = unix_ms();
    if expires > 0 && expires - now < 60_000 {
        return Some(provider("Claude", &plan_label(oauth), &[], None, "Token expired; it will refresh when Claude Code runs"));
    }
    let plan = plan_label(oauth);
    let body = match usage(token) {
        Some(body) => body,
        None => return Some(provider("Claude", &plan, &[], None, "Could not fetch quota")),
    };
    let limits = parse_limits(&body);
    if limits.is_empty() {
        return Some(provider("Claude", &plan, &[], None, "Anthropic sent a quota reply this version doesn't recognize"));
    }
    let extra = body["extra_usage"]["is_enabled"].as_bool().unwrap_or(false).then(|| {
        format!("Extra usage on · {:.0}%", body["extra_usage"]["utilization"].as_f64().unwrap_or(0.0))
    });
    Some(provider("Claude", &plan, &limits, extra.as_deref(), ""))
}

fn usage(token: &str) -> Option<Value> {
    let path = std::env::temp_dir().join(format!("phonebox-quota-{}", std::process::id()));
    {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path).ok()?;
        writeln!(file, "Authorization: Bearer {token}").ok()?;
        writeln!(file, "anthropic-beta: oauth-2025-04-20").ok()?;
    }
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", "15", "-H", &format!("@{}", path.display()), "https://api.anthropic.com/api/oauth/usage"])
        .output();
    let _ = std::fs::remove_file(&path);
    let output = output.ok()?;
    if !output.status.success() { return None }
    serde_json::from_slice(&output.stdout).ok()
}

fn parse_limits(root: &Value) -> Vec<Value> {
    if let Some(limits) = root.get("limits").and_then(Value::as_array) {
        let parsed: Vec<Value> = limits.iter().map(|limit| {
            let kind = limit["kind"].as_str().unwrap_or("");
            let model = limit["scope"]["model"]["display_name"].as_str();
            let (label, window) = match kind {
                "session" => ("5-hour session".to_string(), 5 * 3600),
                "weekly_all" => ("Weekly".to_string(), 7 * 86400),
                "weekly_scoped" => (format!("Weekly · {}", model.unwrap_or("model")), 7 * 86400),
                _ => (if kind.is_empty() { "Limit".into() } else { kind.into() }, 0),
            };
            limit_json(&label, limit["percent"].as_f64().unwrap_or(0.0), limit["resets_at"].as_str(), window, limit["severity"].as_str().unwrap_or("normal"))
        }).collect();
        if !parsed.is_empty() { return parsed }
    }
    let mut legacy = Vec::new();
    for (key, label, window) in [("five_hour", "5-hour session", 18000), ("seven_day", "Weekly", 604800)] {
        if root.get(key).is_some() {
            legacy.push(limit_json(label, root[key]["utilization"].as_f64().unwrap_or(0.0), root[key]["resets_at"].as_str(), window, "normal"));
        }
    }
    legacy
}

fn codex_provider() -> Option<Value> {
    let root = home().join(".codex/sessions");
    if !root.is_dir() { return None }
    let mut files = Vec::new();
    collect_rollouts(&root, &mut files);
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files.truncate(5);
    for (path, modified) in &files {
        let Some(text) = read_tail(path, 1024 * 1024) else { continue };
        if let Some(provider) = codex_from_tail(&text, *modified) { return Some(provider) }
    }
    Some(named_provider("codex", "Codex", "Codex", &[], None, "No quota data in recent sessions", None, None))
}

fn collect_rollouts(dir: &Path, out: &mut Vec<(PathBuf, i64)>) {
    if out.len() >= 200 { return }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= 200 { return }
        let path = entry.path();
        if path.is_dir() { collect_rollouts(&path, out); continue }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") { continue }
        let modified = entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|time| time.as_millis() as i64).unwrap_or(0);
        out.push((path, modified));
    }
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 { return None }
    let take = len.min(max);
    file.seek(SeekFrom::End(-(take as i64))).ok()?;
    let mut buf = vec![0u8; take as usize];
    let read = file.read(&mut buf).ok()?;
    let start = if take < len { buf[..read].iter().position(|byte| *byte == b'\n').map(|i| i + 1).unwrap_or(read) } else { 0 };
    Some(String::from_utf8_lossy(&buf[start..read]).into_owned())
}

fn codex_from_tail(text: &str, modified_ms: i64) -> Option<Value> {
    let mut rate = None;
    for line in text.lines().rev() {
        if !line.contains("\"rate_limits\"") || !line.contains("\"primary\"") { continue }
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if value["payload"]["rate_limits"].is_object() { rate = Some(value); break }
    }
    let rate = rate?;
    let limits_root = &rate["payload"]["rate_limits"];
    let mut limits = Vec::new();
    for key in ["primary", "secondary"] {
        let window = &limits_root[key];
        if !window.is_object() { continue }
        let mins = window["window_minutes"].as_f64().unwrap_or(0.0) as i64;
        let label = match mins {
            300 => "5-hour".to_string(),
            10080 => "Weekly".to_string(),
            n if n >= 1440 => format!("{}-day", n / 1440),
            n if n > 0 => format!("{}-hour", n / 60),
            _ => key.to_string(),
        };
        let resets = window["resets_at"].as_i64().map(|seconds| seconds * 1000)
            .or_else(|| window["resets_in_seconds"].as_f64().map(|seconds| modified_ms + (seconds * 1000.0) as i64));
        let percent = window["used_percent"].as_f64().unwrap_or(0.0);
        let severity = if percent >= 95.0 { "critical" } else if percent >= 80.0 { "warning" } else { "normal" };
        limits.push(limit_at(&label, percent, resets, if mins > 0 { mins * 60 } else { 0 }, severity));
    }
    if limits.is_empty() { return None }
    let mut extra = None;
    let credits = &limits_root["credits"];
    if credits["unlimited"].as_bool().unwrap_or(false) { extra = Some("Credits: unlimited".to_string()); }
    else if credits["has_credits"].as_bool() == Some(false) { extra = Some("Credits not enabled".to_string()); }
    else if let Some(balance) = credits["balance"].as_str().and_then(|text| text.parse::<f64>().ok()) {
        extra = Some(format!("Credits: {balance:.0}"));
    }
    let reached = limits.iter().any(|limit| limit["percent"].as_f64().unwrap_or(0.0) >= 100.0)
        || limits_root["rate_limit_reached_type"].is_string()
        || limits_root["spend_control_reached"].as_bool().unwrap_or(false);
    if reached {
        extra = Some(match extra { Some(text) => format!("Limit reached · {text}"), None => "Limit reached".into() });
    }
    let plan = limits_root["plan_type"].as_str().filter(|plan| !plan.is_empty()).map(|plan| {
        let mut chars = plan.chars();
        match chars.next() { Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(), None => "Codex".into() }
    }).unwrap_or_else(|| "Codex".into());
    let usage = codex_usage(text);
    Some(named_provider("codex", "Codex", &plan, &limits, extra.as_deref(), "", Some(modified_ms), usage))
}

fn codex_usage(text: &str) -> Option<Value> {
    let mut model = None;
    let mut effort = None;
    let mut info = None;
    for line in text.lines().rev() {
        if !line.contains("\"token_count\"") && !line.contains("\"turn_context\"") { continue }
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if info.is_none() && value["payload"]["type"].as_str() == Some("token_count") && value["payload"]["info"].is_object() {
            info = Some(value["payload"]["info"].clone());
        }
        if model.is_none() && value["type"].as_str() == Some("turn_context") {
            model = value["payload"]["model"].as_str().map(str::to_string);
            effort = value["payload"]["effort"].as_str().map(str::to_string);
        }
        if info.is_some() && model.is_some() { break }
    }
    if info.is_none() && model.is_none() { return None }
    let info = info.unwrap_or(Value::Null);
    let total = &info["total_token_usage"];
    let last = &info["last_token_usage"];
    Some(json!({
        "model": model,
        "effort": effort,
        "totalInput": total["input_tokens"].as_i64(),
        "totalCached": total["cached_input_tokens"].as_i64(),
        "totalOutput": total["output_tokens"].as_i64(),
        "totalReasoning": total["reasoning_output_tokens"].as_i64(),
        "totalTokens": total["total_tokens"].as_i64(),
        "lastInput": last["input_tokens"].as_i64(),
        "lastOutput": last["output_tokens"].as_i64(),
        "contextWindow": info["model_context_window"].as_i64(),
    }))
}

fn limit_json(label: &str, percent: f64, resets_at: Option<&str>, window: i64, severity: &str) -> Value {
    limit_at(label, percent, resets_at.and_then(parse_time_ms), window, severity)
}

fn limit_at(label: &str, percent: f64, resets_at: Option<i64>, window: i64, severity: &str) -> Value {
    json!({
        "label": label,
        "percent": percent,
        "resetsAt": resets_at,
        "windowSec": if window > 0 { Value::from(window) } else { Value::Null },
        "severity": severity,
    })
}

fn provider(name: &str, plan: &str, limits: &[Value], extra: Option<&str>, error: &str) -> Value {
    named_provider("claude", name, plan, limits, extra, error, None, None)
}

fn named_provider(id: &str, name: &str, plan: &str, limits: &[Value], extra: Option<&str>, error: &str, updated_at: Option<i64>, codex: Option<Value>) -> Value {
    json!({
        "id": id,
        "name": name,
        "plan": plan,
        "limits": limits,
        "extra": extra,
        "updatedAt": if error.is_empty() { Value::from(updated_at.unwrap_or_else(unix_ms)) } else { Value::Null },
        "error": if error.is_empty() { Value::Null } else { Value::from(error) },
        "source": "local",
        "codex": codex,
    })
}

fn plan_label(oauth: &Value) -> String {
    match oauth["subscriptionType"].as_str() {
        Some("team") => "Team",
        Some("max") => "Max",
        Some("pro") => "Pro",
        Some("enterprise") => "Enterprise",
        Some(other) => return other.to_string(),
        None => "Claude",
    }.to_string()
}

fn parse_time_ms(value: &str) -> Option<i64> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 14 { return None }
    let year = digits[0..4].parse().ok()?;
    let month = digits[4..6].parse().ok()?;
    let day = digits[6..8].parse().ok()?;
    let hour = digits[8..10].parse().ok()?;
    let minute = digits[10..12].parse().ok()?;
    let second = digits[12..14].parse().ok()?;
    Some(crate::feeds::civil_unix(year, month, day, hour, minute, second) * 1000)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_windows_become_limits() {
        let body = json!({"five_hour": {"utilization": 12.5, "resets_at": "2026-11-23T21:00:00Z"}, "seven_day": {"utilization": 40}});
        let limits = parse_limits(&body);
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0]["label"], "5-hour session");
        assert_eq!(limits[0]["percent"], 12.5);
    }

    #[test]
    fn codex_windows_come_from_the_latest_rate_limit() {
        let line = r#"{"payload":{"rate_limits":{"plan_type":"plus","primary":{"used_percent":20,"window_minutes":300,"resets_at":1700000000},"secondary":{"used_percent":80,"window_minutes":10080}}}}"#;
        let card = codex_from_tail(line, 1_700_000_000_000).unwrap();
        assert_eq!(card["id"], "codex");
        assert_eq!(card["plan"], "Plus");
        assert_eq!(card["limits"][0]["label"], "5-hour");
        assert_eq!(card["limits"][1]["label"], "Weekly");
        assert_eq!(card["limits"][1]["severity"], "warning");
    }
}
