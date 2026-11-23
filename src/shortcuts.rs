use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

use serde_json::Value;
#[cfg(test)]
use serde_json::json;

/// Toggle and cycle memory. Lost on restart, same as the original agent.
pub struct Memory {
    remembered: Mutex<HashMap<String, bool>>,
    cycles: Mutex<HashMap<String, Cycle>>,
}

struct Cycle {
    list: String,
    cursor: i32,
    shown: Option<i32>,
    retry: Option<i32>,
}

impl Memory {
    pub fn new() -> Self {
        Self { remembered: Mutex::new(HashMap::new()), cycles: Mutex::new(HashMap::new()) }
    }
}

pub fn toggles(cfg: &Value, muted: Option<bool>, memory: &Memory) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(muted) = muted {
        map.insert("mute".into(), Value::Bool(muted));
    }
    let Ok(remembered) = memory.remembered.lock() else { return Value::Object(map) };
    for shortcut in shortcuts(cfg) {
        let cmd = shortcut["cmd"].as_str().unwrap_or("");
        let toggle = shortcut["toggle"].as_bool().unwrap_or(false) || cmd == "builtin:mute";
        if !toggle { continue }
        let id = shortcut["id"].as_str().unwrap_or("");
        if id.is_empty() { continue }
        let on = if cmd == "builtin:mute" { muted.unwrap_or(false) } else { remembered.get(id).copied().unwrap_or(false) };
        map.insert(id.to_string(), Value::Bool(on));
    }
    Value::Object(map)
}

pub fn cycles(cfg: &Value, memory: &Memory) -> Value {
    let Ok(cycles) = memory.cycles.lock() else { return Value::Null };
    let mut map = serde_json::Map::new();
    for shortcut in shortcuts(cfg) {
        if shortcut["cmd"].as_str() != Some("builtin:cycle") { continue }
        let Some(id) = shortcut["id"].as_str() else { continue };
        let list = list_of(&shortcut);
        let Some(state) = cycles.get(id) else { continue };
        let Some(shown) = state.shown else { continue };
        if state.list != list { continue }
        if let Some(target) = shortcut["cycle"].as_array().and_then(|items| items.get(shown as usize)).and_then(Value::as_str) {
            map.insert(id.to_string(), Value::String(target.to_string()));
        }
    }
    if map.is_empty() { Value::Null } else { Value::Object(map) }
}

/// Runs the shortcut with this id. The phone never sends the command.
pub fn run(cfg: &Value, id: &str, hold: bool, memory: &Memory) -> Result<(), String> {
    let Some(mut shortcut) = shortcuts(cfg).into_iter().find(|item| item["id"].as_str() == Some(id)) else {
        return Err(format!("no such shortcut: {id}"));
    };
    if hold {
        let target = shortcut["longPress"].as_str().ok_or_else(|| format!("shortcut {id} has no long press"))?;
        shortcut = shortcuts(cfg).into_iter().find(|item| item["id"].as_str() == Some(target)).ok_or_else(|| format!("no such shortcut: {target}"))?;
    }
    run_one(cfg, &shortcut, memory, 0)
}

fn run_one(cfg: &Value, shortcut: &Value, memory: &Memory, depth: i32) -> Result<(), String> {
    let cmd = shortcut["cmd"].as_str().unwrap_or("");
    if cmd == "builtin:cycle" {
        return run_cycle(cfg, shortcut, memory, depth);
    }
    let id = shortcut["id"].as_str().unwrap_or("");
    let toggle = shortcut["toggle"].as_bool().unwrap_or(false);
    let on = memory.remembered.lock().map(|map| map.get(id).copied().unwrap_or(false)).unwrap_or(false);
    let (cmd, args, cwd) = if toggle && on && shortcut.get("offCmd").and_then(Value::as_str).is_some() {
        (shortcut["offCmd"].as_str().unwrap_or(""), shortcut["offArgs"].as_str(), shortcut["cwd"].as_str())
    } else {
        (cmd, shortcut["args"].as_str(), shortcut["cwd"].as_str())
    };
    execute(cmd, args, cwd)?;
    if toggle && cmd != "builtin:mute" && shortcut.get("stateKey").and_then(Value::as_str).is_none() {
        if let Ok(mut map) = memory.remembered.lock() {
            map.insert(id.to_string(), !on);
        }
    }
    Ok(())
}

fn run_cycle(cfg: &Value, shortcut: &Value, memory: &Memory, depth: i32) -> Result<(), String> {
    if depth > 3 { return Err("keys call each other too deeply".into()) }
    let ids: Vec<String> = shortcut["cycle"].as_array().map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect()).unwrap_or_default();
    if ids.is_empty() { return Ok(()) }
    let id = shortcut["id"].as_str().unwrap_or("");
    let list = list_of(shortcut);
    let slot = {
        let mut cycles = memory.cycles.lock().map_err(|_| "shortcut state is busy".to_string())?;
        let state = cycles.entry(id.to_string()).or_insert(Cycle { list: list.clone(), cursor: -1, shown: None, retry: None });
        if state.list != list { *state = Cycle { list, cursor: -1, shown: None, retry: None }; }
        let slot = state.retry.take().unwrap_or(state.cursor + 1).rem_euclid(ids.len() as i32);
        state.cursor = slot;
        slot
    };
    let target_id = &ids[slot as usize];
    let err = shortcuts(cfg).into_iter().find(|item| item["id"].as_str() == Some(target_id.as_str()))
        .ok_or_else(|| format!("no such shortcut: {target_id}"))
        .and_then(|target| run_one(cfg, &target, memory, depth + 1));
    if let Ok(mut cycles) = memory.cycles.lock() {
        if let Some(state) = cycles.get_mut(id) {
            if err.is_ok() { state.shown = Some(slot); }
            else { state.retry = Some(slot); }
        }
    }
    err
}

fn execute(cmd: &str, args: Option<&str>, cwd: Option<&str>) -> Result<(), String> {
    let cmd = expand_home(cmd);
    match cmd.as_str() {
        "builtin:mute" => {
            if crate::metrics::toggle_mute() { Ok(()) } else { Err("could not toggle mute on the current output".into()) }
        }
        "builtin:lock" => {
            if command_ok("loginctl", &["lock-session"]) || command_ok("xdg-screensaver", &["lock"]) { Ok(()) } else { Err("could not lock the session".into()) }
        }
        "builtin:monitor-off" => monitor_off(),
        "builtin:keys" | "builtin:text" | "builtin:macro" | "builtin:wake" => {
            Err("this shortcut action is not available yet".into())
        }
        _ => launch(&cmd, args.unwrap_or(""), cwd),
    }
}

fn launch(cmd: &str, args: &str, cwd: Option<&str>) -> Result<(), String> {
    if cmd.contains("://") {
        return open_with("xdg-open", &[cmd], None);
    }
    let (program, args) = match stand_in(cmd) {
        Some(alt) => (alt, String::new()),
        None => (cmd.to_string(), expand_home(args)),
    };
    let mut command = Command::new(&program);
    command.args(split_args(&args));
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd.filter(|dir| !dir.is_empty()) {
        command.current_dir(expand_home(cwd));
    }
    match command.spawn() {
        Ok(child) => {
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            Ok(())
        }
        Err(_) => open_with("xdg-open", &[&program], cwd),
    }
}

fn open_with(program: &str, args: &[&str], cwd: Option<&str>) -> Result<(), String> {
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd.filter(|dir| !dir.is_empty()) {
        command.current_dir(expand_home(cwd));
    }
    command.spawn().map(|_| ()).map_err(|_| "could not start the program".to_string())
}

fn stand_in(cmd: &str) -> Option<String> {
    if which(cmd).is_some() { return None }
    let mut names = Vec::new();
    if cmd.eq_ignore_ascii_case("x-terminal-emulator") {
        if let Ok(terminal) = std::env::var("TERMINAL") {
            let terminal = terminal.trim();
            if !terminal.is_empty() { names.push(terminal.to_string()); }
        }
    }
    names.extend(stand_in_names(cmd).iter().map(|name| (*name).to_string()));
    names.into_iter().find(|name| which(name).is_some())
}

fn stand_in_names(cmd: &str) -> &'static [&'static str] {
    match cmd.trim().to_ascii_lowercase().as_str() {
        "x-terminal-emulator" => &["kgx", "gnome-terminal", "konsole", "xfce4-terminal", "mate-terminal", "lxterminal", "tilix", "alacritty", "kitty", "foot", "wezterm", "xterm"],
        "gnome-calculator" => &["kcalc", "mate-calc", "galculator", "qalculate-gtk", "xfce4-calculator"],
        "gnome-control-center" => &["systemsettings", "systemsettings5", "xfce4-settings-manager", "mate-control-center", "cinnamon-settings", "budgie-control-center", "lxqt-config"],
        "gnome-system-monitor" => &["plasma-systemmonitor", "ksysguard", "xfce4-taskmanager", "mate-system-monitor", "lxtask"],
        _ => &[],
    }
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let candidate = std::path::PathBuf::from(cmd);
    if cmd.contains('/') && candidate.is_file() { return Some(candidate) }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(cmd)).find(|path| path.is_file())
}

fn monitor_off() -> Result<(), String> {
    let commands = monitor_commands(detect_desktop());
    if commands.iter().any(|(program, args)| command_ok(program, args)) {
        Ok(())
    } else {
        Err("could not turn the display off".into())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Desktop {
    Niri,
    Sway,
    Hyprland,
    Gnome,
    Plasma,
    Xfce,
    Mate,
    Cinnamon,
    Lxqt,
    Cosmic,
    Wlroots,
    X11,
    Unknown,
}

fn detect_desktop() -> Desktop {
    let hint = format!(
        "{}:{}",
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        std::env::var("XDG_SESSION_DESKTOP").unwrap_or_default(),
    );
    let from_hint = desktop_from_hint(&hint);
    if from_hint != Desktop::Unknown {
        return from_hint;
    }
    desktop_from_processes(&running_comms())
}

fn desktop_from_hint(hint: &str) -> Desktop {
    let hint = hint.to_ascii_lowercase();
    if hint.contains("niri") { Desktop::Niri }
    else if hint.contains("hypr") { Desktop::Hyprland }
    else if hint.contains("sway") { Desktop::Sway }
    else if hint.contains("cosmic") { Desktop::Cosmic }
    else if hint.contains("kde") || hint.contains("plasma") { Desktop::Plasma }
    else if hint.contains("cinnamon") { Desktop::Cinnamon }
    else if hint.contains("xfce") { Desktop::Xfce }
    else if hint.contains("mate") { Desktop::Mate }
    else if hint.contains("lxqt") || hint.contains("lxde") { Desktop::Lxqt }
    else if hint.contains("gnome") || hint.contains("ubuntu") || hint.contains("budgie") { Desktop::Gnome }
    else if hint.contains("river") || hint.contains("labwc") || hint.contains("wayfire") || hint.contains("weston") { Desktop::Wlroots }
    else { Desktop::Unknown }
}

fn desktop_from_processes(comms: &[String]) -> Desktop {
    let has = |name: &str| comms.iter().any(|comm| comm.eq_ignore_ascii_case(name));
    let mut found = Vec::new();
    if has("niri") { found.push(Desktop::Niri); }
    if has("Hyprland") { found.push(Desktop::Hyprland); }
    if has("sway") { found.push(Desktop::Sway); }
    if has("cosmic-comp") { found.push(Desktop::Cosmic); }
    if has("gnome-shell") { found.push(Desktop::Gnome); }
    if has("plasmashell") || has("kwin_wayland") || has("kwin_x11") { found.push(Desktop::Plasma); }
    if has("cinnamon") || has("muffin") { found.push(Desktop::Cinnamon); }
    if has("xfwm4") { found.push(Desktop::Xfce); }
    if has("marco") { found.push(Desktop::Mate); }
    if has("labwc") || has("river") || has("wayfire") || has("weston") { found.push(Desktop::Wlroots); }
    if has("Xorg") { found.push(Desktop::X11); }
    match found.as_slice() {
        [only] => *only,
        _ => Desktop::Unknown,
    }
}

fn running_comms() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return names };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            names.push(comm.trim().to_string());
        }
    }
    names
}

fn monitor_commands(desktop: Desktop) -> Vec<(&'static str, &'static [&'static str])> {
    let mut commands = Vec::new();
    match desktop {
        Desktop::Niri => commands.push(("niri", &["msg", "action", "power-off-monitors"][..])),
        Desktop::Sway => commands.push(("swaymsg", &["output", "*", "power", "off"][..])),
        Desktop::Hyprland => commands.push(("hyprctl", &["dispatch", "dpms", "off"][..])),
        Desktop::Plasma => commands.push(("kscreen-doctor", &["--dpms", "off"][..])),
        Desktop::Gnome | Desktop::Cinnamon => commands.push(("busctl", &["--user", "call", "org.gnome.ScreenSaver", "/org/gnome/ScreenSaver", "org.gnome.ScreenSaver", "SetActive", "b", "true"][..])),
        Desktop::Xfce | Desktop::Mate | Desktop::Lxqt | Desktop::X11 => commands.push(("xset", &["dpms", "force", "off"][..])),
        Desktop::Cosmic | Desktop::Wlroots => commands.push(("wlopm", &["--off", "*"][..])),
        Desktop::Unknown => {
            commands.extend([
                ("wlopm", &["--off", "*"][..]),
                ("niri", &["msg", "action", "power-off-monitors"][..]),
                ("swaymsg", &["output", "*", "power", "off"][..]),
                ("hyprctl", &["dispatch", "dpms", "off"][..]),
                ("kscreen-doctor", &["--dpms", "off"][..]),
                ("busctl", &["--user", "call", "org.gnome.ScreenSaver", "/org/gnome/ScreenSaver", "org.gnome.ScreenSaver", "SetActive", "b", "true"][..]),
            ]);
        }
    }
    for fallback in [("wlopm", &["--off", "*"][..]), ("xset", &["dpms", "force", "off"][..])] {
        if !commands.iter().any(|(program, _)| *program == fallback.0) {
            commands.push(fallback);
        }
    }
    commands
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).status().map(|status| status.success()).unwrap_or(false)
}

fn shortcuts(cfg: &Value) -> Vec<Value> {
    cfg.get("shortcuts").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn list_of(shortcut: &Value) -> String {
    shortcut["cycle"].as_array().map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default()
}

fn expand_home(text: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if text == "~" || text.starts_with("~/") {
        format!("{home}{}", &text[1..])
    } else {
        text.replace(" ~/", &format!(" {home}/"))
    }
}

fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in text.chars() {
        match (quote, ch) {
            (None, ' ' | '\t') if !current.is_empty() => {
                args.push(std::mem::take(&mut current));
            }
            (None, ' ' | '\t') => {}
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), ch) if ch == open => quote = None,
            _ => current.push(ch),
        }
    }
    if !current.is_empty() { args.push(current); }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_shortcut_is_refused() {
        let cfg = json!({"shortcuts": [{"id": "a", "cmd": "true"}]});
        let memory = Memory::new();
        assert!(run(&cfg, "missing", false, &memory).is_err());
    }

    #[test]
    fn arguments_keep_quoted_spaces() {
        assert_eq!(split_args("one \"two three\""), ["one", "two three"]);
    }

    #[test]
    fn gnome_shortcuts_have_desktop_stand_ins() {
        assert!(stand_in_names("x-terminal-emulator").contains(&"kitty"));
        assert!(stand_in_names("gnome-calculator").contains(&"kcalc"));
        assert!(stand_in_names("gnome-control-center").contains(&"systemsettings"));
        assert!(stand_in_names("spotify").is_empty());
    }

    #[test]
    fn each_desktop_gets_its_own_monitor_command() {
        assert_eq!(monitor_commands(Desktop::Niri)[0].0, "niri");
        assert_eq!(monitor_commands(Desktop::Sway)[0].0, "swaymsg");
        assert_eq!(monitor_commands(Desktop::Hyprland)[0].0, "hyprctl");
        assert_eq!(monitor_commands(Desktop::Plasma)[0].0, "kscreen-doctor");
        assert_eq!(monitor_commands(Desktop::Gnome)[0].0, "busctl");
        assert_eq!(monitor_commands(Desktop::Xfce)[0].0, "xset");
        assert_eq!(monitor_commands(Desktop::Wlroots)[0].0, "wlopm");
        assert_eq!(monitor_commands(Desktop::Sway).last().unwrap().0, "xset");
    }

    #[test]
    fn desktop_hint_beats_a_hardcoded_compositor() {
        assert_eq!(desktop_from_hint("Hyprland"), Desktop::Hyprland);
        assert_eq!(desktop_from_hint("KDE"), Desktop::Plasma);
        assert_eq!(desktop_from_hint("ubuntu:GNOME"), Desktop::Gnome);
        assert_eq!(desktop_from_hint("XFCE"), Desktop::Xfce);
        assert_eq!(desktop_from_processes(&["sway".into(), "niri".into()]), Desktop::Unknown);
        assert_eq!(desktop_from_processes(&["Hyprland".into()]), Desktop::Hyprland);
        assert_eq!(desktop_from_processes(&["sway".into()]), Desktop::Sway);
        assert_eq!(desktop_from_processes(&["gnome-shell".into()]), Desktop::Gnome);
    }
}
