//! Hardware PTT through Linux evdev: real key press and release, independent of the terminal
//! (works over SSH and on the text console). Needs read access to the device (group `input`).

use std::sync::mpsc::Sender;

use crate::app::Msg;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use evdev::{Device, EventType, KeyCode};

    /// "F13", "key_f13" and "KEY_F13" all mean the same key; "BTN_TRIGGER" etc. are accepted too.
    pub fn parse_key(name: &str) -> Result<KeyCode, String> {
        let up = name.trim().to_uppercase();
        let full = if up.starts_with("KEY_") || up.starts_with("BTN_") { up } else { format!("KEY_{up}") };
        full.parse::<KeyCode>().map_err(|_| format!("unbekannte Taste '{name}' (Beispiele: KEY_F13, KEY_PAUSE, BTN_TRIGGER)"))
    }

    /// One line per input device that has keys/buttons: "path<TAB>name".
    pub fn list() -> Vec<String> {
        let mut out: Vec<String> = evdev::enumerate()
            .filter(|(_, d)| d.supported_keys().is_some_and(|k| k.iter().count() > 0))
            .map(|(p, d)| format!("{}\t{}", p.display(), d.name().unwrap_or("?")))
            .collect();
        out.sort();
        out
    }

    /// Opens the device now (so mistakes are reported before the UI starts) and watches it on a thread.
    pub fn spawn(path: &str, key: KeyCode, tx: Sender<Msg>) -> Result<(), String> {
        let mut dev = Device::open(path).map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied { " (Benutzer zur Gruppe 'input' hinzufügen?)" } else { "" };
            format!("{path}: {e}{hint}")
        })?;
        if !dev.supported_keys().is_some_and(|k| k.contains(key)) {
            return Err(format!("{path} ({}) kennt die Taste {key:?} nicht", dev.name().unwrap_or("?")));
        }
        std::thread::spawn(move || {
            loop {
                match dev.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if ev.event_type() == EventType::KEY && ev.code() == key.code() {
                                match ev.value() {
                                    1 => drop(tx.send(Msg::HardwarePtt(true))),
                                    0 => drop(tx.send(Msg::HardwarePtt(false))),
                                    _ => {} // 2 = auto-repeat
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Device unplugged: never leave PTT hanging.
                        let _ = tx.send(Msg::HardwarePtt(false));
                        let _ = tx.send(Msg::Notice(format!("PTT-Gerät verloren: {e}")));
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn key_names_are_forgiving() {
            let f13 = parse_key("KEY_F13").unwrap();
            assert_eq!(parse_key("f13").unwrap(), f13);
            assert_eq!(parse_key("key_f13").unwrap(), f13);
            assert!(parse_key("BTN_TRIGGER").is_ok());
            assert!(parse_key("nonsense").is_err());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    pub struct KeyCode;
    pub fn parse_key(_: &str) -> Result<KeyCode, String> {
        Err("Hardware-PTT gibt es nur unter Linux".into())
    }
    pub fn list() -> Vec<String> {
        Vec::new()
    }
    pub fn spawn(_: &str, _: KeyCode, _: Sender<Msg>) -> Result<(), String> {
        Err("Hardware-PTT gibt es nur unter Linux".into())
    }
}

pub use imp::{list, parse_key, spawn};
