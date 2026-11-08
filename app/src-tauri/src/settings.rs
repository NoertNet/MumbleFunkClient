//! Tiny JSON settings file in the app config dir.
//!
//! Saved servers keep host, port and user name. A password is stored only when the user
//! ticks "remember password"; it is then kept in plain text inside the app's private
//! config directory (OS keystore integration is a later improvement).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Profile {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
}

impl Profile {
    pub fn key(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub profiles: Vec<Profile>,
    /// `Profile::key()` of the most recently used profile.
    pub last: Option<String>,
    /// Audio devices chosen on the desktop (by name); `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Trust-on-first-use certificate fingerprints, key `host:port`, value hex SHA-256.
    pub pins: HashMap<String, String>,
}

impl Settings {
    /// Insert or replace a profile (same user at the same server) and mark it as last used.
    pub fn remember(&mut self, p: Profile) {
        let key = p.key();
        match self.profiles.iter_mut().find(|x| x.key() == key) {
            Some(slot) => *slot = p,
            None => self.profiles.push(p),
        }
        self.last = Some(key);
    }

    pub fn forget(&mut self, key: &str) {
        self.profiles.retain(|p| p.key() != key);
        if self.last.as_deref() == Some(key) {
            self.last = self.profiles.last().map(Profile::key);
        }
    }
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
    let Some(p) = path(app) else { return };
    let Ok(b) = serde_json::to_vec_pretty(s) else { return };
    // Write-then-rename so a crash never leaves a half-written settings file.
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, b).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(user: &str, pw: Option<&str>) -> Profile {
        Profile { host: "h".into(), port: 1, username: user.into(), password: pw.map(Into::into) }
    }

    #[test]
    fn remember_replaces_same_server_and_user() {
        let mut s = Settings::default();
        s.remember(p("a", Some("x")));
        s.remember(p("b", None));
        s.remember(p("a", None)); // user unticked "remember password"
        assert_eq!(s.profiles.len(), 2);
        assert_eq!(s.profiles[0].password, None);
        assert_eq!(s.last.as_deref(), Some("a@h:1"));
    }

    #[test]
    fn forget_moves_last() {
        let mut s = Settings::default();
        s.remember(p("a", None));
        s.remember(p("b", None));
        s.forget("b@h:1");
        assert_eq!(s.last.as_deref(), Some("a@h:1"));
        s.forget("a@h:1");
        assert_eq!(s.last, None);
    }

    #[test]
    fn pin_hex_roundtrip() {
        let b: Vec<u8> = (0..32).collect();
        assert_eq!(parse_pin(&hex(&b)).unwrap().to_vec(), b);
        assert!(parse_pin("zz").is_none());
    }
}
