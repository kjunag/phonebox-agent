use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub struct Feeds {
    calendar: Mutex<Value>,
    github: Mutex<Option<(Instant, Value)>>,
    sessions: Mutex<Option<(Instant, Value)>>,
}

impl Feeds {
    pub fn start(config_path: std::path::PathBuf) -> Arc<Self> {
        let feeds = Arc::new(Self {
            calendar: Mutex::new(json!({"events": [], "updated": 0, "errors": []})),
            github: Mutex::new(None),
            sessions: Mutex::new(None),
        });
        let background = Arc::clone(&feeds);
        std::thread::spawn(move || {
            loop {
                if let Ok(cfg) = crate::http::load_config(&config_path) {
                    let snap = calendar_snapshot(&cfg);
                    if let Ok(mut slot) = background.calendar.lock() {
                        *slot = snap;
                    }
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        });
        feeds
    }

    pub fn calendar(&self) -> Value {
        self.calendar.lock().map(|slot| slot.clone()).unwrap_or_else(|_| json!({"events": [], "updated": 0, "errors": []}))
    }

    pub fn work(&self, cfg: &Value) -> Value {
        json!({
            "sessions": self.sessions(cfg),
            "github": self.github(),
        })
    }

    fn sessions(&self, cfg: &Value) -> Value {
        if let Ok(slot) = self.sessions.lock() {
            if let Some((at, value)) = slot.as_ref() {
                if at.elapsed() < Duration::from_secs(2) {
                    return value.clone();
                }
            }
        }
        let share_titles = cfg.pointer("/claude/shareSessionTitles").and_then(Value::as_bool).unwrap_or(false);
        let mut list = claude_sessions(share_titles);
        if cfg.pointer("/cursor/sessions").and_then(Value::as_bool).unwrap_or(false) {
            list.extend(cursor_sessions());
        }
        list.extend(codex_sessions());
        let value = Value::Array(list);
        if let Ok(mut slot) = self.sessions.lock() {
            *slot = Some((Instant::now(), value.clone()));
        }
        value
    }

    fn github(&self) -> Value {
        if let Ok(slot) = self.github.lock() {
            if let Some((at, value)) = slot.as_ref() {
                if at.elapsed() < Duration::from_secs(180) {
                    return value.clone();
                }
            }
        }
        let fresh = fetch_github();
        if let Ok(mut slot) = self.github.lock() {
            *slot = Some((Instant::now(), fresh.clone()));
        }
        fresh
    }
}

fn calendar_snapshot(cfg: &Value) -> Value {
    let sources = cfg.get("calendars").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut events = Vec::new();
    let mut errors = Vec::new();
    let now = unix_secs();
    let from = now - 86_400;
    let to = now + 14 * 86_400;
    for source in sources {
        let name = source.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).unwrap_or("Calendar");
        let url = source.get("url").and_then(Value::as_str).unwrap_or("").trim();
        if url.is_empty() { continue }
        let color = source.get("color").and_then(Value::as_str).unwrap_or("#60a5fa");
        match fetch_text(url) {
            Ok(text) => events.extend(parse_ics(&text, name, color, from, to)),
            Err(_) => errors.push(format!("{name}: unavailable")),
        }
    }
    events.sort_by(|a, b| a["start"].as_i64().unwrap_or(0).cmp(&b["start"].as_i64().unwrap_or(0)));
    json!({"events": events, "updated": unix_ms(), "errors": errors})
}

fn fetch_text(url: &str) -> Result<String, ()> {
    let url = if let Some(rest) = url.strip_prefix("webcal://") { format!("https://{rest}") } else { url.to_string() };
    if !(url.starts_with("https://") || url.starts_with("http://")) { return Err(()) }
    let output = Command::new("curl").args(["-fsSL", "--max-time", "20", "-A", "PhoneBox", &url]).output().map_err(|_| ())?;
    if !output.status.success() { return Err(()) }
    String::from_utf8(output.stdout).map_err(|_| ())
}

fn parse_ics(text: &str, calendar: &str, color: &str, from: i64, to: i64) -> Vec<Value> {
    let unfolded = unfold(text);
    let mut events = Vec::new();
    for block in unfolded.split("BEGIN:VEVENT").skip(1) {
        let body = block.split("END:VEVENT").next().unwrap_or("");
        let mut summary = String::new();
        let mut location = None;
        let mut start = None;
        let mut end = None;
        let mut all_day = false;
        let mut rule = None;
        for line in body.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let name = key.split(';').next().unwrap_or(key);
            match name {
                "SUMMARY" => summary = unescape(value),
                "LOCATION" => location = Some(unescape(value)),
                "DTSTART" => {
                    all_day = !value.contains('T');
                    start = parse_time(value);
                }
                "DTEND" => end = parse_time(value),
                "RRULE" => rule = Some(value.to_string()),
                _ => {}
            }
        }
        let Some(start) = start else { continue };
        let end = end.unwrap_or(start + if all_day { 86_400 } else { 3600 });
        for (start, end) in expand(start, end, rule.as_deref(), from, to) {
            events.push(json!({
                "title": summary,
                "start": start * 1000,
                "end": end * 1000,
                "allDay": all_day,
                "location": location,
                "calendar": calendar,
                "color": color,
            }));
        }
    }
    events
}

fn unfold(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.pop();
            out.push_str(line.trim_start());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn unescape(value: &str) -> String {
    value.replace("\\n", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\")
}

fn parse_time(value: &str) -> Option<i64> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 { return None }
    let year: i32 = digits[0..4].parse().ok()?;
    let month: u32 = digits[4..6].parse().ok()?;
    let day: u32 = digits[6..8].parse().ok()?;
    let (hour, minute, second) = if digits.len() >= 14 {
        (digits[8..10].parse().ok()?, digits[10..12].parse().ok()?, digits[12..14].parse().ok()?)
    } else {
        (0, 0, 0)
    };
    Some(civil_unix(year, month, day, hour, minute, second))
}

pub(crate) fn civil_unix(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    let mut y = year;
    let mut m = month as i32;
    if m <= 2 { y -= 1; m += 12; }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (m - 3) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u64;
    let days = (era as i64) * 146097 + doe as i64 - 719468;
    days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
}

fn expand(start: i64, end: i64, rule: Option<&str>, from: i64, to: i64) -> Vec<(i64, i64)> {
    let Some(rule) = rule else {
        return (end >= from && start <= to).then_some((start, end)).into_iter().collect();
    };
    let mut freq = "";
    let mut interval = 1i64;
    let mut count = None;
    let mut until = None;
    let mut byday: Vec<i64> = Vec::new();
    for part in rule.split(';') {
        let Some((key, value)) = part.split_once('=') else { continue };
        match key {
            "FREQ" => freq = value,
            "INTERVAL" => interval = value.parse().unwrap_or(1).max(1),
            "COUNT" => count = value.parse().ok(),
            "UNTIL" => until = parse_time(value),
            "BYDAY" => byday = value.split(',').filter_map(weekday).collect(),
            _ => {}
        }
    }
    let step = match freq {
        "DAILY" => 86_400 * interval,
        "WEEKLY" => 7 * 86_400 * interval,
        "MONTHLY" => 30 * 86_400 * interval,
        "YEARLY" => 365 * 86_400 * interval,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut at = start;
    let mut seen = 0i64;
    if step > 0 && at + (end - start) < from {
        let jumps = (from - at) / step;
        at += jumps * step;
        seen += jumps;
    }
    let limit = until.unwrap_or(to);
    while at <= limit && seen < count.unwrap_or(400) && out.len() < 200 {
        let stop = at + (end - start);
        if stop >= from && at <= to && (byday.is_empty() || byday.contains(&weekday_of(at))) {
            out.push((at, stop));
        }
        seen += 1;
        at += step;
    }
    out
}

fn weekday(token: &str) -> Option<i64> {
    let day = token.trim_start_matches(|c: char| c == '-' || c.is_ascii_digit());
    Some(match day { "MO" => 1, "TU" => 2, "WE" => 3, "TH" => 4, "FR" => 5, "SA" => 6, "SU" => 0, _ => return None })
}

fn weekday_of(unix: i64) -> i64 {
    // 1970-01-01 was a Thursday.
    (unix.div_euclid(86_400) + 4).rem_euclid(7)
}

fn claude_sessions(share_titles: bool) -> Vec<Value> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let dir = home.join(".claude/sessions");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().filter_map(|entry| {
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".json") { return None }
        let text = std::fs::read_to_string(entry.path()).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let pid = value.get("pid").and_then(Value::as_i64)?;
        if !Path::new(&format!("/proc/{pid}")).exists() { return None }
        let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or("");
        if cwd.contains("/CodexBar/ClaudeProbe") || cwd.contains("/.claude-mem/observer-sessions/") { return None }
        let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or("");
        let session_name = value.get("name").and_then(Value::as_str).unwrap_or(project);
        Some(json!({
            "id": format!("claude:{}", value.get("sessionId").and_then(Value::as_str).unwrap_or(&pid.to_string())),
            "tool": "claude",
            "name": session_name,
            "project": project,
            "status": value.get("status").and_then(Value::as_str).unwrap_or("idle"),
            "waitingFor": value.get("waitingFor").cloned().unwrap_or(Value::Null),
            "since": value.get("statusUpdatedAt").and_then(Value::as_i64).or_else(|| value.get("updatedAt").and_then(Value::as_i64)).unwrap_or(0),
            "startedAt": value.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
            "pid": pid,
            "title": if share_titles { value.get("title").cloned().unwrap_or(Value::Null) } else { Value::Null },
        }))
    }).collect()
}

fn cursor_sessions() -> Vec<Value> {
    let home = home_dir();
    let names = cursor_project_names(&home);
    let cutoff = unix_secs() - 3 * 3600;
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(home.join(".cursor/projects")) else { return out };
    for project in projects.flatten() {
        let slug = project.file_name().to_string_lossy().to_string();
        let Ok(chats) = std::fs::read_dir(project.path().join("agent-transcripts")) else { continue };
        for chat in chats.flatten() {
            let id = chat.file_name().to_string_lossy().to_string();
            let Ok(files) = std::fs::read_dir(chat.path()) else { continue };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") { continue }
                if path.to_string_lossy().contains("/.agent-data-cleanup-") { continue }
                let Ok(meta) = file.metadata() else { continue };
                let modified = system_secs(meta.modified().ok());
                if modified < cutoff { continue }
                let project_name = names.get(&slug).cloned().unwrap_or_else(|| cursor_fallback(&slug));
                let short = if id.len() >= 4 { &id[..4] } else { id.as_str() };
                let age = unix_secs() - modified;
                let role = last_jsonl_string(&path, "role");
                let status = if age < 180 && role.as_deref() != Some("assistant") { "busy" } else { "idle" };
                out.push(json!({
                    "id": format!("cursor:{id}"),
                    "tool": "cursor",
                    "name": format!("{}-{short}", project_name.to_lowercase()),
                    "project": project_name,
                    "status": status,
                    "waitingFor": null,
                    "since": modified * 1000,
                    "startedAt": system_secs(meta.created().ok()) * 1000,
                    "title": null,
                }));
            }
        }
    }
    out
}

fn cursor_project_names(home: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(dirs) = std::fs::read_dir(home.join(".config/Cursor/User/workspaceStorage")) else { return map };
    for dir in dirs.flatten() {
        let Ok(text) = std::fs::read_to_string(dir.path().join("workspace.json")) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(folder) = value.get("folder").and_then(Value::as_str) else { continue };
        let Some(path) = file_uri_path(folder) else { continue };
        let slug = cursor_slug(&path);
        if slug.is_empty() { continue }
        let trimmed = path.trim_end_matches('/').to_string();
        let name = Path::new(&trimmed).file_name().and_then(|n| n.to_str()).unwrap_or("Cursor").to_string();
        map.insert(slug, name);
    }
    map
}

fn cursor_slug(path: &str) -> String {
    path.trim_end_matches('/').trim_start_matches('/').replace('/', "-").replace(':', "")
}

fn cursor_fallback(slug: &str) -> String {
    if slug.is_empty() || slug.chars().all(|ch| ch.is_ascii_digit()) { return "Cursor".into() }
    slug.split('-').filter(|part| !part.is_empty()).next_back().unwrap_or("Cursor").to_string()
}

fn file_uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    Some(percent_decode(rest))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn codex_sessions() -> Vec<Value> {
    let root = home_dir().join(".codex/sessions");
    let cutoff = unix_secs() - 3 * 3600;
    let mut files = Vec::new();
    collect_rollouts(&root, cutoff, &mut files);
    files.truncate(40);
    files.into_iter().filter_map(|(path, modified, created)| {
        let cwd = codex_cwd(&path);
        let project = Path::new(&cwd).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("codex");
        let stamp = if name.len() > 8 { &name[name.len().saturating_sub(14)..name.len().saturating_sub(6)] } else { name };
        let short = if stamp.len() >= 4 { &stamp[..4] } else { stamp };
        let meta = codex_tail(&path);
        let age = unix_secs() - modified;
        let status = if meta.lifecycle == "task_started" && age < 180 { "busy" } else { "idle" };
        let outcome = match meta.lifecycle.as_str() {
            "task_complete" => Some("completed"),
            "turn_aborted" => Some("aborted"),
            _ => None,
        };
        Some(json!({
            "id": format!("codex:{name}"),
            "tool": "codex",
            "name": format!("{}-{short}", project.to_lowercase()),
            "project": project,
            "status": status,
            "waitingFor": null,
            "since": meta.last_activity.unwrap_or(modified * 1000),
            "startedAt": created * 1000,
            "model": meta.model,
            "effort": meta.effort,
            "totalTokens": meta.total_tokens,
            "inputTokens": meta.input_tokens,
            "outputTokens": meta.output_tokens,
            "cachedInputTokens": meta.cached_input_tokens,
            "reasoningTokens": meta.reasoning_tokens,
            "lastInputTokens": meta.last_input_tokens,
            "contextWindow": meta.context_window,
            "turnStartedAt": meta.turn_started,
            "outcome": outcome,
            "lastTool": meta.last_tool,
            "title": null,
        }))
    }).collect()
}

struct CodexMeta {
    model: Option<String>,
    effort: Option<String>,
    total_tokens: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    last_input_tokens: Option<i64>,
    context_window: Option<i64>,
    last_activity: Option<i64>,
    turn_started: Option<i64>,
    lifecycle: String,
    last_tool: Option<String>,
}

fn codex_cwd(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let mut buf = vec![0u8; 64 * 1024];
    let Ok(read) = file.read(&mut buf) else { return String::new() };
    let Some(end) = buf[..read].iter().position(|b| *b == b'\n') else { return String::new() };
    let Ok(value) = serde_json::from_slice::<Value>(&buf[..end]) else { return String::new() };
    value["payload"]["cwd"].as_str().unwrap_or("").to_string()
}

fn codex_tail(path: &Path) -> CodexMeta {
    let mut meta = CodexMeta {
        model: None, effort: None, total_tokens: None, input_tokens: None, output_tokens: None,
        cached_input_tokens: None, reasoning_tokens: None, last_input_tokens: None, context_window: None,
        last_activity: None, turn_started: None, lifecycle: String::new(), last_tool: None,
    };
    let Ok(text) = read_tail(path, 512 * 1024) else { return meta };
    for line in text.lines() {
        if !line.contains("\"event_msg\"") && !line.contains("\"turn_context\"") && !line.contains("\"function_call\"") && !line.contains("\"custom_tool_call\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let kind = value["type"].as_str().unwrap_or("");
        let payload = &value["payload"];
        let event = payload["type"].as_str().unwrap_or("");
        let stamp = value["timestamp"].as_str().map(parse_rfc3339_ms).filter(|n| *n != 0);
        if kind == "event_msg" {
            if event != "token_count" { meta.last_activity = stamp.or(meta.last_activity); }
            if event == "token_count" {
                let total = &payload["info"]["total_token_usage"];
                let last = &payload["info"]["last_token_usage"];
                meta.total_tokens = total["total_tokens"].as_i64().or(meta.total_tokens);
                meta.input_tokens = total["input_tokens"].as_i64().or(meta.input_tokens);
                meta.output_tokens = total["output_tokens"].as_i64().or(meta.output_tokens);
                meta.cached_input_tokens = total["cached_input_tokens"].as_i64().or(meta.cached_input_tokens);
                meta.reasoning_tokens = total["reasoning_output_tokens"].as_i64().or(meta.reasoning_tokens);
                meta.last_input_tokens = last["input_tokens"].as_i64().or(meta.last_input_tokens);
                meta.context_window = payload["info"]["model_context_window"].as_i64().or(meta.context_window);
            }
            if matches!(event, "task_started" | "task_complete" | "turn_aborted") {
                meta.lifecycle = event.to_string();
                if event == "task_started" { meta.turn_started = stamp.or(meta.turn_started); }
            }
        } else if kind == "turn_context" {
            meta.model = payload["model"].as_str().map(str::to_string).or(meta.model);
            meta.effort = payload["effort"].as_str().map(str::to_string).or(meta.effort);
        } else if kind == "response_item" && matches!(event, "function_call" | "custom_tool_call") {
            meta.last_tool = payload["name"].as_str().map(str::to_string).or(meta.last_tool);
        }
    }
    meta
}

fn collect_rollouts(dir: &Path, cutoff: i64, out: &mut Vec<(PathBuf, i64, i64)>) {
    if out.len() >= 40 { return }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= 40 { return }
        let path = entry.path();
        if path.is_dir() { collect_rollouts(&path, cutoff, out); continue }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") { continue }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = system_secs(meta.modified().ok());
        if modified < cutoff { continue }
        out.push((path, modified, system_secs(meta.created().ok())));
    }
}

fn last_jsonl_string(path: &Path, key: &str) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    let mut max = 8 * 1024u64;
    loop {
        let text = read_tail(path, max).ok()?;
        let mut found = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || !line.starts_with('{') { continue }
            let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
            if let Some(role) = value.get(key).and_then(Value::as_str) {
                found = Some(role.to_string());
            }
        }
        if found.is_some() || max >= len || max >= 1024 * 1024 { return found }
        max = (max * 2).min(len).min(1024 * 1024);
    }
}

fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 { return Ok(String::new()) }
    let take = len.min(max);
    file.seek(SeekFrom::End(-(take as i64)))?;
    let mut buf = vec![0u8; take as usize];
    let read = file.read(&mut buf)?;
    let mut start = 0;
    if take < len {
        start = buf[..read].iter().position(|b| *b == b'\n').map(|i| i + 1).unwrap_or(read);
    }
    Ok(String::from_utf8_lossy(&buf[start..read]).into_owned())
}

fn system_secs(time: Option<SystemTime>) -> i64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|time| time.as_secs() as i64).unwrap_or(0)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn fetch_github() -> Value {
    let query = r#"query { viewer { login pullRequests(states: OPEN, first: 8, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { number title isDraft reviewDecision updatedAt repository { nameWithOwner } commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } } } } search(query: "is:pr is:open archived:false review-requested:@me", type: ISSUE, first: 8) { nodes { ... on PullRequest { number title isDraft updatedAt author { login } repository { nameWithOwner } } } } }"#;
    let Ok(output) = Command::new("timeout").args(["20", "gh", "api", "graphql", "-f", &format!("query={query}")]).output() else {
        return github_error("gh is not available");
    };
    if !output.status.success() { return github_error("gh is not logged in") }
    let Ok(doc) = serde_json::from_slice::<Value>(&output.stdout) else { return github_error("gh returned an unexpected answer") };
    let data = &doc["data"];
    let viewer = &data["viewer"];
    let mine = viewer["pullRequests"]["nodes"].as_array().map(|nodes| {
        nodes.iter().map(|node| gh_pr(node, None, node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str(), node["reviewDecision"].as_str())).collect::<Vec<_>>()
    }).unwrap_or_default();
    let reviews = data["search"]["nodes"].as_array().map(|nodes| {
        nodes.iter().filter(|node| node.get("number").is_some()).map(|node| gh_pr(node, node["author"]["login"].as_str(), None, None)).collect::<Vec<_>>()
    }).unwrap_or_default();
    let notifications = Command::new("timeout").args(["20", "gh", "api", "notifications?per_page=50"]).output().ok()
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .and_then(|value| value.as_array().map(|items| items.len() as i64))
        .unwrap_or(0);
    json!({
        "user": viewer["login"].as_str(),
        "notifications": notifications,
        "reviewRequests": reviews,
        "myPrs": mine,
        "updatedAt": unix_ms(),
        "error": null,
    })
}

fn gh_pr(node: &Value, author: Option<&str>, ci: Option<&str>, review: Option<&str>) -> Value {
    json!({
        "repo": node["repository"]["nameWithOwner"].as_str().unwrap_or(""),
        "number": node["number"].as_i64().unwrap_or(0),
        "title": node["title"].as_str().unwrap_or(""),
        "author": author,
        "draft": node["isDraft"].as_bool().unwrap_or(false),
        "review": review,
        "ci": ci,
        "updated": parse_rfc3339_ms(node["updatedAt"].as_str().unwrap_or("")),
    })
}

fn github_error(message: &str) -> Value {
    json!({"user": null, "notifications": 0, "reviewRequests": [], "myPrs": [], "updatedAt": null, "error": message})
}

fn parse_rfc3339_ms(value: &str) -> i64 {
    // 2026-11-23T18:00:00Z
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 14 { return 0 }
    let year = digits[0..4].parse().unwrap_or(1970);
    let month = digits[4..6].parse().unwrap_or(1);
    let day = digits[6..8].parse().unwrap_or(1);
    let hour = digits[8..10].parse().unwrap_or(0);
    let minute = digits[10..12].parse().unwrap_or(0);
    let second = digits[12..14].parse().unwrap_or(0);
    civil_unix(year, month, day, hour, minute, second) * 1000
}

fn unix_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_secs() as i64).unwrap_or(0)
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_event_stays_inside_two_weeks() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Standup\nDTSTART:20261102T090000Z\nDTEND:20261102T093000Z\nRRULE:FREQ=WEEKLY;BYDAY=MO\nEND:VEVENT\nEND:VCALENDAR\n";
        let from = civil_unix(2026, 11, 16, 0, 0, 0);
        let to = from + 14 * 86_400;
        let events = parse_ics(ics, "Work", "#60a5fa", from, to);
        assert!(events.iter().all(|event| event["title"] == "Standup"));
        assert!(events.iter().all(|event| weekday_of(event["start"].as_i64().unwrap() / 1000) == 1));
        assert!(!events.is_empty());
    }

    #[test]
    fn cursor_role_is_the_last_record() {
        let path = std::env::temp_dir().join(format!("phonebox-role-{}.jsonl", std::process::id()));
        std::fs::write(&path, "{\"role\":\"user\"}\n{\"role\":\"assistant\"}\n").unwrap();
        assert_eq!(last_jsonl_string(&path, "role").as_deref(), Some("assistant"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cursor_slug_matches_the_project_folder() {
        assert_eq!(cursor_slug("/home/szpadel/Dokumenty/phonebox"), "home-szpadel-Dokumenty-phonebox");
    }
}
