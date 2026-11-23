use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

static SAVE: Mutex<()> = Mutex::new(());

use serde_json::{json, Value};

pub const CONFLICT: &str = "The settings were changed in another tab or window after this one loaded them. Reload the page to see them; the changes made here were not saved.";

pub fn public_settings(cfg: &Value, desktop: bool) -> Value {
    let mut cfg = cfg.clone();
    if !desktop {
        if cfg.get("steam").is_some() {
            if let Some(steam) = cfg.get_mut("steam") {
                if let Some(object) = steam.as_object_mut() {
                    object.remove("apiKey");
                }
            }
        }
    }
    cfg
}

pub fn prepare_save(incoming: &mut Value, existing: &Value, local_port: u16) -> Result<(), String> {
    if !incoming.is_object() { return Err("Invalid JSON".into()) }
    if lan_port_collides(incoming, local_port) {
        return Err(format!("Wi-Fi port cannot be the same as the local port ({local_port})"));
    }
    if keys_shared(incoming) {
        return Err("The Wi-Fi key and the webhook key must be different: make a new key for one of them".into());
    }
    if let Some(lat) = incoming.pointer("/weather/lat").and_then(Value::as_f64) {
        if !(-90.0..=90.0).contains(&lat) { return Err("Invalid weather location".into()) }
    }
    if let Some(lon) = incoming.pointer("/weather/lon").and_then(Value::as_f64) {
        if !(-180.0..=180.0).contains(&lon) { return Err("Invalid weather location".into()) }
    }
    for key in ["/night/start", "/night/end"] {
        if let Some(time) = incoming.pointer(key).and_then(Value::as_str) {
            if !hhmm(time) { return Err("Night mode times must be in HH:MM format".into()) }
        }
    }
    if let Some(shortcuts) = incoming.get("shortcuts").and_then(Value::as_array) {
        for shortcut in shortcuts {
            if shortcut.get("cmd").and_then(Value::as_str).unwrap_or("").trim().is_empty() {
                let label = shortcut.get("label").and_then(Value::as_str).unwrap_or("shortcut");
                return Err(format!("Shortcut '{label}' has no command"));
            }
        }
    }
    preserve_steam_key(incoming, existing);
    Ok(())
}

pub fn apply_note(path: &Path, page_id: &str, widget_id: &str, text: Option<&str>, version: Option<&str>) -> Result<String, (u16, String)> {
    let Some(text) = text else { return Err((400, "text expected".into())) };
    if text.chars().count() > 500 { return Err((400, "text too long".into())) }
    let mut cfg = crate::http::load_config(path).map_err(|_| (400, "could not read settings".to_string()))?;
    let widget = cfg.get_mut("pages").and_then(Value::as_array_mut).and_then(|pages| {
        pages.iter_mut().find(|page| page.get("id").and_then(Value::as_str) == Some(page_id))
    }).and_then(|page| page.get_mut("widgets")).and_then(Value::as_array_mut).and_then(|widgets| {
        widgets.iter_mut().find(|widget| widget.get("id").and_then(Value::as_str) == Some(widget_id))
    });
    let Some(widget) = widget else { return Err((404, "widget not found".into())) };
    if widget.get("type").and_then(Value::as_str) != Some("note") { return Err((400, "option not allowed".into())) }
    if !widget.get("options").is_some_and(Value::is_object) { widget["options"] = json!({}); }
    widget["options"]["text"] = Value::String(text.to_string());
    let expected = version.filter(|value| !value.is_empty()).map(str::to_string).unwrap_or_else(|| crate::http::config_version(path));
    save(path, &cfg, Some(expected.as_str())).map_err(|error| {
        if error == CONFLICT { (409, error) } else { (400, error) }
    })
}

pub fn save(path: &Path, cfg: &Value, expected: Option<&str>) -> Result<String, String> {
    let _guard = SAVE.lock().map_err(|_| "could not save settings".to_string())?;
    if let Some(expected) = expected.filter(|value| !value.is_empty()) {
        if expected != crate::http::config_version(path) { return Err(CONFLICT.into()) }
    }
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|_| "could not save settings".to_string())?; }
    if path.exists() {
        let backup = path.with_extension("json.bak");
        let _ = fs::copy(path, &backup);
        let _ = fs::set_permissions(&backup, private_mode());
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|_| "could not save settings".to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp).map_err(|_| "could not save settings".to_string())?;
        file.write_all(text.as_bytes()).map_err(|_| "could not save settings".to_string())?;
    }
    fs::rename(&tmp, path).map_err(|_| "could not save settings".to_string())?;
    let _ = fs::set_permissions(path, private_mode());
    Ok(crate::http::config_version(path))
}

fn preserve_steam_key(incoming: &mut Value, existing: &Value) {
    let Some(key) = existing.pointer("/steam/apiKey").and_then(Value::as_str).filter(|key| !key.is_empty()) else { return };
    let missing = incoming.pointer("/steam/apiKey").and_then(Value::as_str).map(str::is_empty).unwrap_or(true);
    if !missing { return }
    if !incoming.get("steam").is_some_and(Value::is_object) { incoming["steam"] = json!({}); }
    incoming["steam"]["apiKey"] = Value::String(key.to_string());
}

fn lan_port_collides(cfg: &Value, local_port: u16) -> bool {
    cfg.pointer("/lan/enabled").and_then(Value::as_bool).unwrap_or(false)
        && cfg.pointer("/lan/port").and_then(Value::as_u64) == Some(u64::from(local_port))
}

fn keys_shared(cfg: &Value) -> bool {
    let phone = cfg.pointer("/lan/token").and_then(Value::as_str).unwrap_or("");
    let hook = cfg.pointer("/lan/webhookToken").and_then(Value::as_str).unwrap_or("");
    !phone.is_empty() && phone == hook
}

fn hhmm(text: &str) -> bool {
    let mut parts = text.split(':');
    let Some(hour) = parts.next().and_then(|part| part.parse::<u32>().ok()) else { return false };
    let Some(minute) = parts.next().and_then(|part| part.parse::<u32>().ok()) else { return false };
    parts.next().is_none() && hour < 24 && minute < 60 && text.len() == 5
}

fn private_mode() -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(0o600)
}

pub fn catalog() -> String {
    format!(
        "{{\"cols\":12,\"rows\":6,\"widgets\":{},\"templates\":{}}}",
        include_str!("../assets/widgets.json"),
        include_str!("../assets/templates.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_wifi_and_webhook_keys_are_refused() {
        let mut cfg = json!({"lan": {"enabled": true, "port": 8766, "token": "same", "webhookToken": "same"}});
        let err = prepare_save(&mut cfg, &json!({}), 8765).unwrap_err();
        assert!(err.contains("must be different"));
    }

    #[test]
    fn a_missing_steam_key_stays_on_the_computer() {
        let mut cfg = json!({"steam": {"enabled": true}});
        prepare_save(&mut cfg, &json!({"steam": {"apiKey": "secret"}}), 8765).unwrap();
        assert_eq!(cfg["steam"]["apiKey"], "secret");
    }

    #[test]
    fn another_tabs_version_is_not_overwritten() {
        let path = std::env::temp_dir().join(format!("phonebox-settings-{}.json", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        let err = save(&path, &json!({"pages": []}), Some("1")).unwrap_err();
        assert_eq!(err, CONFLICT);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_note_changes_only_its_text() {
        let path = std::env::temp_dir().join(format!("phonebox-note-{}.json", std::process::id()));
        std::fs::write(&path, r##"{"lan":{"token":"keep"},"pages":[{"id":"focus","widgets":[{"id":"note","type":"note","options":{"text":"old","color":"#fff"}},{"id":"clock","type":"clock"}]}]}"##).unwrap();
        let version = crate::http::config_version(&path);
        let saved = apply_note(&path, "focus", "note", Some("hello"), Some(&version)).unwrap();
        let cfg = crate::http::load_config(&path).unwrap();
        assert_eq!(cfg["pages"][0]["widgets"][0]["options"]["text"], "hello");
        assert_eq!(cfg["pages"][0]["widgets"][0]["options"]["color"], "#fff");
        assert_eq!(cfg["lan"]["token"], "keep");
        assert_eq!(apply_note(&path, "focus", "clock", Some("x"), Some(&saved)).unwrap_err().0, 400);
        assert_eq!(apply_note(&path, "focus", "missing", Some("x"), Some(&saved)).unwrap_err().0, 404);
        let long = "x".repeat(501);
        assert_eq!(apply_note(&path, "focus", "note", Some(&long), Some(&saved)).unwrap_err().0, 400);
        let _ = std::fs::remove_file(path);
    }
}
