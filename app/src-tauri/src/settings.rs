//! Tiny JSON settings file in the app config dir. Passwords are never stored.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    /// Trust-on-first-use certificate fingerprints, key `host:port`, value hex SHA-256.
    pub pins: HashMap<String, String>,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("settings.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, s: &Settings) {
    if let Some(p) = path(app) {
        if let Ok(b) = serde_json::to_vec_pretty(s) {
            let _ = std::fs::write(p, b);
        }
    }
}

pub fn parse_pin(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
