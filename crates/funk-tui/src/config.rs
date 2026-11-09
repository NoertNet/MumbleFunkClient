//! Settings of the terminal client. Kept in its own file, separate from the GUI app's
//! `settings.json`, so the two can never overwrite each other's fields.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    /// Only stored if the user asked for it with `--save-password` (file mode 0600).
    pub password: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub ptt_device: Option<String>,
    pub ptt_key: Option<String>,
    /// Trust-on-first-use certificate fingerprints, key `host:port`, value hex SHA-256.
    pub pins: HashMap<String, String>,
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("mumble-funk").join("cli.json")
}

pub fn load_from(path: &Path) -> Config {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Write-then-rename so a crash never leaves a half-written file; readable by the owner only.
pub fn save_to(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg).map_err(std::io::Error::other)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("funk-tui-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("cli.json")
    }

    #[test]
    fn roundtrip_and_permissions() {
        let p = tmp("rt");
        let mut c = Config { host: "h".into(), port: Some(64739), username: "u".into(), ..Default::default() };
        c.pins.insert("h:64739".into(), "ab".repeat(32));
        save_to(&p, &c).unwrap();
        assert_eq!(load_from(&p), c);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn missing_or_broken_file_gives_defaults() {
        assert_eq!(load_from(&tmp("missing")), Config::default());
        let p = tmp("broken");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{ not json").unwrap();
        assert_eq!(load_from(&p), Config::default());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn unknown_fields_in_the_file_are_tolerated() {
        let p = tmp("unknown");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, br#"{"host":"x","somethingNew":1}"#).unwrap();
        assert_eq!(load_from(&p).host, "x");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn pin_hex_roundtrip() {
        let b: Vec<u8> = (0..32).collect();
        assert_eq!(parse_pin(&hex(&b)).unwrap().to_vec(), b);
        assert!(parse_pin("zz").is_none());
    }
}
