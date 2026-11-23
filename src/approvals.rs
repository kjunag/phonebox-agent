use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::oneshot;

struct Pending {
    approval: Value,
    tx: oneshot::Sender<String>,
}

pub struct Approvals {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Approvals {
    pub fn new() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }

    pub fn snapshot(&self) -> Value {
        let now = unix_ms();
        let Ok(pending) = self.pending.lock() else { return json!([]) };
        let mut items: Vec<Value> = pending.values().map(|item| item.approval.clone()).filter(|item| item["expiresAt"].as_i64().unwrap_or(0) > now).collect();
        items.sort_by_key(|item| item["createdAt"].as_i64().unwrap_or(0));
        Value::Array(items)
    }

    /// Shows the card on a phone that polled recently and waits for its answer.
    pub async fn ask(&self, approval: Value, phone_seen: i64) -> Option<String> {
        let now = unix_ms();
        let expires = approval.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
        if now.saturating_sub(phone_seen) >= 5_000 || expires <= now {
            return None;
        }
        let id = approval.get("id").and_then(Value::as_str)?.to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().ok()?.insert(id.clone(), Pending { approval, tx });
        let decision = tokio::time::timeout(Duration::from_millis((expires - now) as u64), rx).await.ok().and_then(Result::ok);
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
        decision
    }

    pub fn decide(&self, id: &str, decision: &str, value: Option<&str>) -> bool {
        let Ok(mut pending) = self.pending.lock() else { return false };
        let Some(item) = pending.get(id) else { return false };
        let now = unix_ms();
        if item.approval["expiresAt"].as_i64().unwrap_or(0) <= now {
            return false;
        }
        let Some(result) = accepted(&item.approval, decision, value.unwrap_or(""), now) else { return false };
        let Some(item) = pending.remove(id) else { return false };
        item.tx.send(result).is_ok()
    }
}

fn accepted(approval: &Value, decision: &str, value: &str, now: i64) -> Option<String> {
    if approval["kind"].as_str() == Some("question") {
        let options = approval["options"].as_array();
        let custom = approval["custom"].as_bool().unwrap_or(false);
        if decision == "skip" && options.map(|items| items.is_empty()).unwrap_or(true) && !custom {
            return Some("skip".into());
        }
        if decision == "answer" && !value.is_empty() && options.is_some_and(|items| items.iter().any(|item| item["value"].as_str() == Some(value))) {
            return Some(value.to_string());
        }
        if decision == "answer" && custom {
            let typed = value.trim();
            let typed = if typed.chars().count() > 500 { typed.chars().take(500).collect() } else { typed.to_string() };
            if !typed.is_empty() { return Some(typed) }
        }
        return None;
    }
    match decision {
        "allow" | "deny" => Some(decision.to_string()),
        "always" if approval["always"].as_bool().unwrap_or(false) && now >= approval["alwaysNotBefore"].as_i64().unwrap_or(0) => Some("always".into()),
        _ => None,
    }
}

pub async fn handle(input: &Value, cfg: &Value, approvals: &Approvals, phone_seen: i64, source: &str, allow_questions: bool) -> Option<Value> {
    if !input.is_object() { return None }
    let claude = &cfg["claude"];
    if claude["phoneApprovals"].as_bool() != Some(true) { return None }
    if claude["onlyWhenAway"].as_bool().unwrap_or(false) && idle_seconds() < 60.0 { return None }
    let tool = input["tool_name"].as_str().unwrap_or("a tool");
    let tool_input = input.get("tool_input").filter(|value| value.is_object()).cloned().unwrap_or(Value::Null);
    let cwd = input["cwd"].as_str().unwrap_or("").trim();
    let session = input["session_id"].as_str().unwrap_or("");
    let question = allow_questions && tool == "AskUserQuestion";
    let now = unix_ms();
    let timeout = claude["timeoutSec"].as_i64().unwrap_or(90).clamp(10, 3600);
    let approval = json!({
        "id": new_id(),
        "source": source,
        "session": session,
        "project": project_name(cwd),
        "cwd": shorten(cwd, 1000),
        "tool": tool_title(tool),
        "detail": if question { question_detail(&tool_input) } else { detail(tool, &tool_input) },
        "description": tool_input.get("description").and_then(Value::as_str),
        "createdAt": now,
        "expiresAt": now + timeout * 1000,
        "kind": if question { "question" } else { "permission" },
        "options": null,
        "custom": false,
        "always": false,
        "alwaysNotBefore": 0,
        "alwaysPattern": null,
    });
    let decision = approvals.ask(approval, phone_seen).await?;
    if !matches!(decision.as_str(), "allow" | "always" | "deny") { return None }
    let body = if decision == "deny" {
        json!({"behavior": "deny", "message": "Denied from the PhoneBox panel."})
    } else {
        json!({"behavior": "allow"})
    };
    Some(json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": body}}))
}

pub fn install_claude_hook(port: u16, timeout_sec: i64) -> Result<bool, String> {
    install_hook(port, timeout_sec, &claude_settings_path(), "/api/hooks/claude/permission")
}

fn install_hook(port: u16, timeout_sec: i64, path: &Path, endpoint: &str) -> Result<bool, String> {
    let mut root = if path.exists() {
        let raw = fs::read_to_string(path).map_err(|_| "could not read the hook settings".to_string())?;
        serde_json::from_str(&raw).map_err(|_| "hook settings are not a JSON object".to_string())?
    } else {
        json!({})
    };
    if !root.is_object() { return Err("hook settings are not a JSON object".into()) }
    if root.get("hooks").is_some() && !root["hooks"].is_object() { return Err("hooks is not an object".into()) }
    if root["hooks"].get("PermissionRequest").is_some() && !root["hooks"]["PermissionRequest"].is_array() {
        return Err("PermissionRequest is not a list".into());
    }
    let groups = root["hooks"]["PermissionRequest"].as_array().cloned().unwrap_or_default();
    let command = hook_command(port, endpoint);
    let timeout = timeout_sec.clamp(10, 3600) + 15;
    let mut kept = Vec::new();
    let mut same = false;
    for group in groups {
        let Some(hooks) = group.get("hooks").and_then(Value::as_array) else { kept.push(group); continue };
        let mut filtered = Vec::new();
        for hook in hooks {
            if is_ours(hook, endpoint) {
                same |= hook["command"].as_str() == Some(command.as_str()) && hook["timeout"].as_i64() == Some(timeout);
            } else {
                filtered.push(hook.clone());
            }
        }
        if filtered.is_empty() { continue }
        let mut group = group;
        group["hooks"] = Value::Array(filtered);
        kept.push(group);
    }
    if same { return Ok(false) }
    kept.push(json!({
        "matcher": "*",
        "hooks": [{ "type": "command", "command": command, "timeout": timeout }],
    }));
    if !root["hooks"].is_object() { root["hooks"] = json!({}); }
    root["hooks"]["PermissionRequest"] = Value::Array(kept);
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|_| "could not create the hook settings folder".to_string())?; }
    write_private(path, &serde_json::to_string_pretty(&root).map_err(|_| "could not write the hook settings".to_string())?)?;
    Ok(true)
}

fn is_ours(hook: &Value, endpoint: &str) -> bool {
    let Some(command) = hook["command"].as_str() else { return false };
    command.match_indices("127.0.0.1:").any(|(pos, host)| {
        let rest = &command[pos + host.len()..];
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        digits > 0 && rest[digits..].starts_with(endpoint)
            && rest[digits + endpoint.len()..].chars().next().is_none_or(|ch| !is_path_char(ch))
    })
}

fn is_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | '~' | '%')
}

fn hook_command(port: u16, endpoint: &str) -> String {
    format!("curl -s --noproxy 127.0.0.1 --max-time 3600 -X POST http://127.0.0.1:{port}{endpoint} -H \"Content-Type: application/json\" --data-binary @-")
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp).map_err(|_| "could not write the hook settings".to_string())?;
        std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|_| "could not write the hook settings".to_string())?;
    }
    fs::rename(&tmp, path).map_err(|_| "could not replace the hook settings".to_string())
}

fn claude_settings_path() -> PathBuf {
    home().join(".claude/settings.json")
}

fn tool_title(tool: &str) -> String {
    match tool {
        "Bash" => "Run a command".into(),
        "Edit" | "MultiEdit" => "Edit a file".into(),
        "Write" => "Write a file".into(),
        "NotebookEdit" => "Edit a notebook".into(),
        "WebFetch" => "Fetch a web page".into(),
        "WebSearch" => "Search the web".into(),
        "Read" => "Read a file".into(),
        "AskUserQuestion" => "Claude has a question".into(),
        _ if tool.starts_with("mcp__") => format!("Use {}", tool[5..].replace("__", " › ")),
        _ => format!("Use {tool}"),
    }
}

fn detail(tool: &str, input: &Value) -> String {
    if !input.is_object() { return String::new() }
    let mut text = match tool {
        "Bash" => input["command"].as_str().map(str::to_string),
        "Edit" | "MultiEdit" | "Read" | "NotebookEdit" => input["file_path"].as_str().or_else(|| input["notebook_path"].as_str()).map(str::to_string),
        "Write" => input["file_path"].as_str().map(|path| format!("{path}\n\n{}", head(input["content"].as_str().unwrap_or(""), 12))),
        "WebFetch" => input["url"].as_str().map(str::to_string),
        "WebSearch" => input["query"].as_str().map(str::to_string),
        _ => None,
    };
    if tool == "Edit" {
        if let (Some(body), Some(edit)) = (text.as_mut(), input["new_string"].as_str()) {
            body.push_str("\n\n");
            body.push_str(&head(edit, 12));
        }
    }
    shorten(&text.unwrap_or_else(|| serde_json::to_string_pretty(input).unwrap_or_default()), 3000)
}

fn question_detail(input: &Value) -> String {
    let mut text = String::new();
    if let Some(questions) = input.get("questions").and_then(Value::as_array) {
        for question in questions {
            if !text.is_empty() { text.push_str("\n\n"); }
            if let Some(header) = question["header"].as_str().filter(|header| !header.is_empty()) {
                text.push_str(header);
                text.push_str(": ");
            }
            text.push_str(question["question"].as_str().unwrap_or(""));
            if question["multiSelect"].as_bool().unwrap_or(false) { text.push_str("  (more than one can be picked)"); }
            if let Some(options) = question.get("options").and_then(Value::as_array) {
                for option in options {
                    let Some(label) = option["label"].as_str().filter(|label| !label.is_empty()) else { continue };
                    text.push_str("\n  • ");
                    text.push_str(label);
                    if let Some(description) = option["description"].as_str().filter(|description| !description.is_empty()) {
                        text.push_str(" — ");
                        text.push_str(description);
                    }
                }
            }
        }
    }
    if text.is_empty() { text = "Claude is asking something; answer it in the terminal on your PC.".into(); }
    shorten(&text, 3000)
}

fn head(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.split('\n').collect();
    let mut shown = all.iter().take(lines).copied().collect::<Vec<_>>().join("\n");
    if all.len() > lines { shown.push_str(&format!("\n… ({} more lines)", all.len() - lines)); }
    shown
}

fn project_name(cwd: &str) -> String {
    Path::new(cwd.trim_end_matches('/')).file_name().and_then(|name| name.to_str()).unwrap_or("").to_string()
}

fn shorten(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max { return text.to_string() }
    let body: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{body}…")
}

fn idle_seconds() -> f64 {
    std::process::Command::new("xprintidle").output().ok().filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .map(|ms| ms / 1000.0)
        .unwrap_or(0.0)
}

fn new_id() -> String {
    let mut bytes = [0u8; 6];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut bytes);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_phone_skips_the_prompt() {
        let store = Approvals::new();
        let card = json!({"id": "abc", "expiresAt": unix_ms() + 5_000, "kind": "permission"});
        assert!(store.ask(card, 0).await.is_none());
    }

    #[tokio::test]
    async fn allow_from_the_phone_is_returned() {
        let store = std::sync::Arc::new(Approvals::new());
        let waiting = std::sync::Arc::clone(&store);
        let card = json!({"id": "abc", "expiresAt": unix_ms() + 2_000, "kind": "permission", "always": false});
        let task = tokio::spawn(async move { waiting.ask(card, unix_ms()).await });
        for _ in 0..50 {
            if !store.snapshot().as_array().unwrap().is_empty() { break }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(store.decide("abc", "allow", None));
        assert_eq!(task.await.unwrap().as_deref(), Some("allow"));
    }

    #[test]
    fn bash_detail_is_the_command() {
        let text = detail("Bash", &json!({"command": "echo hi"}));
        assert_eq!(text, "echo hi");
        assert_eq!(tool_title("Bash"), "Run a command");
    }

    #[test]
    fn hook_recognizes_only_its_own_endpoint() {
        let hook = json!({"command": "curl http://127.0.0.1:8765/api/hooks/claude/permission"});
        assert!(is_ours(&hook, "/api/hooks/claude/permission"));
        assert!(!is_ours(&hook, "/api/hooks/codex/permission"));
    }
}
