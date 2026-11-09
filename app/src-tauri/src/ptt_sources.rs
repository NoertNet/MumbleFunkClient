//! Who is holding push-to-talk? The on-screen button/keyboard (`ui`) and the global hotkey are
//! separate sources; the radio transmits while *either* is held and stops only when both are up.
//! Pure logic (no I/O), so every path is unit tested.

/// What to do with the transmitter after a source changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Start,
    Stop,
}

#[derive(Default)]
pub struct PttSources {
    ui: bool,
    hotkey: bool,
    /// Identifies the current hotkey press so a stale watchdog cannot release a newer one.
    epoch: u64,
}

impl PttSources {
    pub fn transmitting(&self) -> bool {
        self.ui || self.hotkey
    }

    fn change(&mut self, f: impl FnOnce(&mut Self)) -> Effect {
        let before = self.transmitting();
        f(self);
        match (before, self.transmitting()) {
            (false, true) => Effect::Start,
            (true, false) => Effect::Stop,
            _ => Effect::None,
        }
    }

    /// On-screen PTT button or keyboard in the window.
    pub fn ui(&mut self, down: bool) -> Effect {
        self.change(|s| s.ui = down)
    }

    /// Global hotkey pressed. Repeated "pressed" reports of one hold are ignored. Returns the epoch for
    /// the transmit-limit watchdog when this press is new.
    pub fn hotkey_down(&mut self) -> (Effect, Option<u64>) {
        if self.hotkey {
            return (Effect::None, None);
        }
        self.epoch += 1;
        let epoch = self.epoch;
        (self.change(|s| s.hotkey = true), Some(epoch))
    }

    /// Global hotkey released. Windows can report several releases for one hold; only the first counts.
    pub fn hotkey_up(&mut self) -> Effect {
        if !self.hotkey {
            return Effect::None;
        }
        self.epoch += 1;
        self.change(|s| s.hotkey = false)
    }

    /// The transmit limit of a hotkey press ran out: release it, unless that press is already over.
    pub fn hotkey_expired(&mut self, epoch: u64) -> Effect {
        if self.hotkey && self.epoch == epoch {
            self.epoch += 1;
            return self.change(|s| s.hotkey = false);
        }
        Effect::None
    }

    /// Connection ended: forget everything.
    pub fn reset(&mut self) -> Effect {
        self.epoch += 1;
        self.change(|s| {
            s.ui = false;
            s.hotkey = false;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_hold_starts_and_stops_once() {
        let mut p = PttSources::default();
        assert_eq!(p.hotkey_down().0, Effect::Start);
        assert!(p.transmitting());
        assert_eq!(p.hotkey_up(), Effect::Stop);
        assert!(!p.transmitting());
    }

    #[test]
    fn repeated_pressed_reports_do_not_restart_or_renew_the_epoch() {
        let mut p = PttSources::default();
        let (e, epoch) = p.hotkey_down();
        assert_eq!((e, epoch.is_some()), (Effect::Start, true));
        for _ in 0..5 {
            assert_eq!(p.hotkey_down(), (Effect::None, None));
        }
        assert_eq!(p.hotkey_expired(epoch.unwrap()), Effect::Stop, "epoch of the first press is still valid");
    }

    #[test]
    fn a_burst_of_releases_stops_only_once() {
        let mut p = PttSources::default();
        p.hotkey_down();
        assert_eq!(p.hotkey_up(), Effect::Stop);
        for _ in 0..4 {
            assert_eq!(p.hotkey_up(), Effect::None);
        }
    }

    #[test]
    fn ui_held_keeps_transmitting_when_the_hotkey_is_released() {
        let mut p = PttSources::default();
        assert_eq!(p.ui(true), Effect::Start);
        assert_eq!(p.hotkey_down().0, Effect::None, "already transmitting");
        assert_eq!(p.hotkey_up(), Effect::None, "button still held");
        assert!(p.transmitting());
        assert_eq!(p.ui(false), Effect::Stop);
    }

    #[test]
    fn hotkey_held_keeps_transmitting_when_the_ui_lets_go() {
        // e.g. the window loses focus and the UI releases its own PTT
        let mut p = PttSources::default();
        p.hotkey_down();
        assert_eq!(p.ui(true), Effect::None);
        assert_eq!(p.ui(false), Effect::None, "hotkey still held");
        assert!(p.transmitting());
        assert_eq!(p.hotkey_up(), Effect::Stop);
    }

    #[test]
    fn stale_watchdog_cannot_release_a_newer_press() {
        let mut p = PttSources::default();
        let first = p.hotkey_down().1.unwrap();
        p.hotkey_up();
        let second = p.hotkey_down().1.unwrap();
        assert_ne!(first, second);
        assert_eq!(p.hotkey_expired(first), Effect::None, "old press timer must not stop the new one");
        assert!(p.transmitting());
        assert_eq!(p.hotkey_expired(second), Effect::Stop);
    }

    #[test]
    fn expiry_after_release_does_nothing() {
        let mut p = PttSources::default();
        let e = p.hotkey_down().1.unwrap();
        p.hotkey_up();
        assert_eq!(p.hotkey_expired(e), Effect::None);
    }

    #[test]
    fn reset_stops_and_forgets() {
        let mut p = PttSources::default();
        p.hotkey_down();
        p.ui(true);
        assert_eq!(p.reset(), Effect::Stop);
        assert!(!p.transmitting());
        assert_eq!(p.reset(), Effect::None);
    }
}
