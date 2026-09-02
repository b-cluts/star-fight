//! Client config on disk: remembered certificate pins (host:port →
//! fingerprint) and the last-used menu values. Plain text, one entry per
//! line, in the platform config directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn config_dir() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("starfight")
}

pub fn pins_path() -> PathBuf {
    config_dir().join("pins.txt")
}

fn menu_path() -> PathBuf {
    config_dir().join("menu.txt")
}

fn load_map(path: &PathBuf) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once(char::is_whitespace) {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn save_map(path: &PathBuf, header: &str, map: &BTreeMap<String, String>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut out = format!("# {header}\n");
    for (k, v) in map {
        out.push_str(&format!("{k} {v}\n"));
    }
    let _ = std::fs::write(path, out);
}

/// Remembered fingerprint for `host:port`, if any.
pub fn pin_for(key: &str) -> Option<String> {
    load_map(&pins_path()).remove(key)
}

/// Remember (or lengthen) the pin for `host:port`.
pub fn remember_pin(key: &str, fingerprint: &str) {
    let path = pins_path();
    let mut map = load_map(&path);
    if map.get(key).map(|v| v.as_str()) == Some(fingerprint) {
        return;
    }
    map.insert(key.to_string(), fingerprint.to_string());
    save_map(
        &path,
        "Star Fight remembered server certificates: host:port sha256-fingerprint. \
         Delete a line to forget a server.",
        &map,
    );
}

/// Last-used menu values (name, server address) — never the password.
pub fn load_menu() -> BTreeMap<String, String> {
    load_map(&menu_path())
}

pub fn save_menu(name: &str, addr: &str) {
    let mut map = BTreeMap::new();
    map.insert("name".to_string(), name.to_string());
    map.insert("server".to_string(), addr.to_string());
    save_map(&menu_path(), "Star Fight last-used menu values", &map);
}
