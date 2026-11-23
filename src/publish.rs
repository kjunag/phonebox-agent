use serde_json::{json, Map, Value};

/// The slice of `config.json` the phone may see. Commands, tokens and camera addresses stay here.
pub fn public_config(root: &Value, device_id: Option<&str>) -> Value {
    let pages = pages_for(root, device_id)
        .into_iter()
        .filter_map(|page| page.as_object().map(public_page))
        .collect::<Vec<_>>();
    json!({
        "weather": root.get("weather").cloned().unwrap_or(Value::Null),
        "codexBar": root.get("codexBar").cloned().unwrap_or(Value::Null),
        "night": root.get("night").cloned().unwrap_or(Value::Null),
        "display": root.get("display").cloned().unwrap_or(Value::Null),
        "pages": pages,
        "cameras": cameras(root),
        "shortcuts": shortcuts(root),
        "rules": rules(root),
        "charge": charge(root),
        "opencode": { "alwaysCountdownSec": root.pointer("/opencode/alwaysCountdownSec").cloned().unwrap_or(Value::Null) },
    })
}

fn pages_for(root: &Value, device_id: Option<&str>) -> Vec<Value> {
    let pages = root.get("pages").and_then(Value::as_array).cloned().unwrap_or_default();
    let Some(device_id) = device_id.filter(|id| !id.is_empty()) else { return pages };
    let wanted = root
        .get("phones")
        .and_then(Value::as_array)
        .and_then(|phones| phones.iter().find(|phone| phone.get("id").and_then(Value::as_str) == Some(device_id)))
        .and_then(|phone| phone.get("pages"))
        .and_then(Value::as_array);
    let Some(wanted) = wanted else { return pages };
    let chosen: Vec<Value> = pages
        .iter()
        .filter(|page| page.get("id").is_some_and(|id| wanted.iter().any(|wanted_id| wanted_id == id)))
        .cloned()
        .collect();
    if chosen.is_empty() { pages } else { chosen }
}

fn public_page(page: &Map<String, Value>) -> Value {
    let widgets = page
        .get("widgets")
        .and_then(Value::as_array)
        .map(|widgets| widgets.iter().map(public_widget).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({
        "id": page.get("id").cloned().unwrap_or(Value::Null),
        "title": page.get("title").cloned().unwrap_or(Value::Null),
        "icon": page.get("icon").cloned().unwrap_or(Value::Null),
        "enabled": page.get("enabled").cloned().unwrap_or(Value::Bool(true)),
        "idleSec": page.get("idleSec").cloned().unwrap_or(Value::Null),
        "appearance": page.get("appearance").cloned().unwrap_or(Value::Null),
        "widgets": widgets,
    })
}

fn public_widget(widget: &Value) -> Value {
    let kind = widget.get("type").and_then(Value::as_str).unwrap_or("");
    json!({
        "id": widget.get("id").cloned().unwrap_or(Value::Null),
        "type": widget.get("type").cloned().unwrap_or(Value::Null),
        "x": widget.get("x").cloned().unwrap_or(Value::Null),
        "y": widget.get("y").cloned().unwrap_or(Value::Null),
        "w": widget.get("w").cloned().unwrap_or(Value::Null),
        "h": widget.get("h").cloned().unwrap_or(Value::Null),
        "appearance": widget.get("appearance").cloned().unwrap_or(Value::Null),
        "options": public_options(kind, widget.get("options")),
    })
}

fn public_options(kind: &str, options: Option<&Value>) -> Value {
    if kind == "command" {
        let mut kept = Map::new();
        if let Some(map) = options.and_then(Value::as_object) {
            for key in ["title", "interval"] {
                if let Some(value) = map.get(key) {
                    kept.insert(key.to_string(), value.clone());
                }
            }
        }
        return Value::Object(kept);
    }
    match options {
        Some(Value::Object(map)) => Value::Object(
            map.iter()
                .filter_map(|(key, value)| {
                    if secret_key(key) {
                        None
                    } else if key == "widgets" {
                        Some((key.clone(), public_children(value)))
                    } else {
                        Some((key.clone(), strip_secrets(value)))
                    }
                })
                .collect(),
        ),
        Some(other) => strip_secrets(other),
        None => Value::Null,
    }
}

fn public_children(value: &Value) -> Value {
    let Some(children) = value.as_array() else { return strip_secrets(value) };
    Value::Array(children.iter().map(public_widget).collect())
}

fn strip_secrets(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !secret_key(key))
                .map(|(key, child)| (key.clone(), strip_secrets(child)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_secrets).collect()),
        other => other.clone(),
    }
}

fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "command" | "commandunix" | "commandwindows" | "cmd" | "args" | "cwd" | "script" | "token" | "password" | "secret" | "apikey" | "api_key" | "webhook" | "webhooktoken"
    ) || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
}

fn shortcuts(root: &Value) -> Value {
    let Some(items) = root.get("shortcuts").and_then(Value::as_array) else { return json!([]) };
    Value::Array(
        items
            .iter()
            .map(|item| {
                let cmd = item.get("cmd").and_then(Value::as_str).unwrap_or("");
                json!({
                    "id": item.get("id").cloned().unwrap_or(Value::Null),
                    "label": item.get("label").cloned().unwrap_or(Value::Null),
                    "icon": item.get("icon").cloned().unwrap_or(Value::Null),
                    "color": item.get("color").cloned().unwrap_or(Value::Null),
                    "toggle": item.get("toggle").and_then(Value::as_bool).unwrap_or(false) || cmd == "builtin:mute",
                    "hold": item.get("longPress").is_some_and(|value| !value.is_null()),
                    "cycle": cmd == "builtin:cycle" && item.get("cycle").and_then(Value::as_array).is_some_and(|c| !c.is_empty()),
                    "iconRev": Value::Null,
                    "iconUpload": false,
                })
            })
            .collect(),
    )
}

fn rules(root: &Value) -> Value {
    let Some(items) = root.get("rules").and_then(Value::as_array) else { return json!([]) };
    Value::Array(
        items
            .iter()
            .map(|item| {
                json!({
                    "id": item.get("id").cloned().unwrap_or(Value::Null),
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "enabled": item.get("enabled").cloned().unwrap_or(Value::Null),
                    "when": item.get("when").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

fn cameras(root: &Value) -> Value {
    let Some(items) = root.pointer("/cameras/sources").and_then(Value::as_array) else { return json!([]) };
    Value::Array(
        items
            .iter()
            .map(|item| json!({ "id": item.get("id").cloned().unwrap_or(Value::Null), "fps": item.get("fps").cloned().unwrap_or(Value::Null) }))
            .collect(),
    )
}

fn charge(root: &Value) -> Value {
    json!({
        "enabled": root.pointer("/charge/enabled").cloned().unwrap_or(Value::Null),
        "min": root.pointer("/charge/min").cloned().unwrap_or(Value::Null),
        "max": root.pointer("/charge/max").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_off_the_phone() {
        let root = json!({
            "lan": { "token": "super-secret-token" },
            "webhookToken": "hook-secret",
            "pages": [{ "id": "home", "widgets": [
                { "id": "w1", "type": "command", "options": { "title": "Hi", "command": "rm -rf /" } }
            ]}],
            "shortcuts": [{ "id": "s", "label": "Mute", "cmd": "builtin:mute" }],
            "cameras": { "sources": [{ "id": "cam", "fps": 1, "url": "rtsp://user:pass@cam" }] }
        });
        let text = public_config(&root, None).to_string();
        assert!(!text.contains("super-secret-token"));
        assert!(!text.contains("hook-secret"));
        assert!(!text.contains("rm -rf"));
        assert!(!text.contains("rtsp://"));
        assert!(text.contains("Hi"));
    }
}
