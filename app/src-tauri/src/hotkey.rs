//! Global PTT hotkey (desktop): rules for which keys are allowed, and registration.
//!
//! A global hotkey is swallowed by the system: no other program sees the key any more. So keys that
//! people type with (Space, letters, Ctrl+C, ...) are refused; what remains are keys that are free in
//! practice. Windows reports a key press and, by polling every 50 ms, its release, which is what
//! hold-to-talk needs.

use std::str::FromStr;

use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

use crate::settings::HotkeySpec;

/// Maximum time a hotkey may keep the transmitter open if the release is never seen.
pub const TX_LIMIT: std::time::Duration = std::time::Duration::from_secs(90);

fn is_f_key(code: Code) -> bool {
    use Code::*;
    matches!(
        code,
        F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 | F13 | F14 | F15 | F16 | F17 | F18 | F19 | F20 | F21 | F22 | F23 | F24
    )
}

fn is_letter_or_digit(code: Code) -> bool {
    let name = format!("{code:?}");
    (name.starts_with("Key") && name.len() == 4) || (name.starts_with("Digit") && name.len() == 6)
}

/// Checks a recorded hotkey against the rules and turns it into modifiers + key.
pub fn validate(spec: &HotkeySpec) -> Result<(Modifiers, Code), String> {
    let code = Code::from_str(&spec.code).map_err(|_| format!("Unbekannte Taste: {}", spec.code))?;
    if code == Code::Space {
        return Err("Die Leertaste kann kein globaler Hotkey sein: sie würde in allen Programmen blockiert.".into());
    }
    if code == Code::F12 {
        return Err("F12 ist von Windows reserviert und lässt sich nicht als Hotkey belegen.".into());
    }
    let mut mods = Modifiers::empty();
    mods.set(Modifiers::CONTROL, spec.ctrl);
    mods.set(Modifiers::ALT, spec.alt);
    mods.set(Modifiers::SHIFT, spec.shift);
    mods.set(Modifiers::SUPER, spec.meta);
    let count = [spec.ctrl, spec.alt, spec.shift, spec.meta].iter().filter(|&&m| m).count();
    let special = is_f_key(code) || matches!(code, Code::Pause | Code::ScrollLock);
    let typing = is_letter_or_digit(code);
    if !special && !typing {
        return Err("Diese Taste wird nicht unterstützt. Nimm eine F-Taste, Pause, Rollen, einen Buchstaben oder eine Ziffer (die beiden letzten nur mit Zusatztasten).".into());
    }
    if count == 0 && !special {
        return Err("Eine einzelne Buchstaben- oder Zifferntaste würde überall im System blockiert. Nimm eine F-Taste (z. B. F9) oder eine Kombination wie Strg+Alt+T.".into());
    }
    if count == 1 && typing {
        return Err("Mit nur einer Zusatztaste würden Kürzel wie Strg+C oder Strg+V im ganzen System blockiert. Nimm eine F-Taste oder mindestens zwei Zusatztasten, z. B. Strg+Alt+T.".into());
    }
    Ok((mods, code))
}

/// Human readable form: "Strg+Alt+T", "F9".
pub fn describe(spec: &HotkeySpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    if spec.ctrl {
        parts.push("Strg".into());
    }
    if spec.alt {
        parts.push("Alt".into());
    }
    if spec.shift {
        parts.push("Umschalt".into());
    }
    if spec.meta {
        parts.push("Win".into());
    }
    let key = spec.code.strip_prefix("Key").or_else(|| spec.code.strip_prefix("Digit")).unwrap_or(&spec.code);
    parts.push(if key == "ScrollLock" { "Rollen".into() } else { key.to_string() });
    parts.join("+")
}

pub fn shortcut(spec: &HotkeySpec) -> Result<Shortcut, String> {
    let (mods, code) = validate(spec)?;
    Ok(Shortcut::new(if mods.is_empty() { None } else { Some(mods) }, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(code: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> HotkeySpec {
        HotkeySpec { code: code.into(), ctrl, alt, shift, meta }
    }

    #[test]
    fn space_is_refused_in_every_form() {
        for (c, a, s) in [(false, false, false), (true, false, false), (true, true, false), (true, true, true)] {
            let r = validate(&spec("Space", c, a, s, false));
            assert!(r.unwrap_err().contains("Leertaste"), "Space with mods {c}{a}{s}");
        }
    }

    #[test]
    fn f12_is_refused() {
        assert!(validate(&spec("F12", false, false, false, false)).unwrap_err().contains("F12"));
    }

    #[test]
    fn single_typing_keys_are_refused() {
        for code in ["KeyA", "KeyZ", "Digit1"] {
            assert!(validate(&spec(code, false, false, false, false)).is_err(), "{code}");
        }
    }

    #[test]
    fn one_modifier_plus_typing_key_is_refused() {
        for (c, a, s, m) in [(true, false, false, false), (false, true, false, false), (false, false, true, false), (false, false, false, true)] {
            assert!(validate(&spec("KeyC", c, a, s, m)).is_err());
            assert!(validate(&spec("Digit5", c, a, s, m)).is_err());
        }
    }

    #[test]
    fn allowed_combinations() {
        for (code, c, a, s) in [
            ("F9", false, false, false),
            ("F1", false, false, false),
            ("F11", false, false, false),
            ("F13", false, false, false),
            ("F24", false, false, false),
            ("Pause", false, false, false),
            ("ScrollLock", false, false, false),
            ("F9", true, false, false),
            ("KeyT", true, true, false),
            ("KeyT", true, false, true),
            ("Digit7", false, true, true),
        ] {
            assert!(validate(&spec(code, c, a, s, false)).is_ok(), "{code} {c}{a}{s} should be allowed");
        }
    }

    #[test]
    fn punctuation_enter_tab_arrows_are_refused_even_with_modifiers() {
        for code in ["Comma", "Semicolon", "Enter", "Tab", "Backspace", "Escape", "ArrowUp", "Quote", "BracketLeft"] {
            assert!(validate(&spec(code, true, true, false, false)).is_err(), "{code}");
        }
    }

    #[test]
    fn unknown_names_are_refused() {
        assert!(validate(&spec("NotAKey", true, true, false, false)).unwrap_err().contains("Unbekannte"));
    }

    /// Exactly the strings the UI sends must be understood by the key parser.
    #[test]
    fn ui_key_names_parse() {
        let mut names: Vec<String> = ('A'..='Z').map(|c| format!("Key{c}")).collect();
        names.extend((0..=9).map(|d| format!("Digit{d}")));
        names.extend((1..=24).map(|n| format!("F{n}")));
        names.extend(["Pause".to_string(), "ScrollLock".to_string()]);
        for n in names {
            assert!(Code::from_str(&n).is_ok(), "{n}");
        }
    }

    #[test]
    fn descriptions_are_readable() {
        assert_eq!(describe(&spec("F9", false, false, false, false)), "F9");
        assert_eq!(describe(&spec("KeyT", true, true, false, false)), "Strg+Alt+T");
        assert_eq!(describe(&spec("Digit7", false, true, true, false)), "Alt+Umschalt+7");
        assert_eq!(describe(&spec("ScrollLock", false, false, false, false)), "Rollen");
    }

    #[test]
    fn shortcut_builds_for_valid_input() {
        assert!(shortcut(&spec("KeyT", true, true, false, false)).is_ok());
        assert!(shortcut(&spec("Space", true, true, false, false)).is_err());
    }
}
