//! Push-to-talk state machine. Pure logic (no I/O) so every path is unit tested.
//!
//! Three kinds of input feed it:
//! * terminal key with release events (kitty keyboard protocol) -> `Hold`
//! * terminal key without release events                        -> `Toggle`
//! * hardware key via evdev (real press + release)              -> always hold
//!
//! A hard limit on transmit time releases a stuck PTT (lost key release, SSH hiccup, ...).

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// Press starts, release stops (terminal reports key releases).
    Hold,
    /// Press starts, next press stops (terminal does not report releases).
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Released,
    /// Maximum transmit time reached.
    Limit,
    /// Program ends / connection lost.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Start,
    Stop(StopReason),
}

/// Key repeat on terminals without release events arrives as a stream of presses; presses closer than
/// this to the previous key event are treated as repeats.
const DEBOUNCE: Duration = Duration::from_millis(250);

pub struct Ptt {
    mode: KeyMode,
    limit: Duration,
    active_since: Option<Instant>,
    last_key_event: Option<Instant>,
}

impl Ptt {
    pub fn new(mode: KeyMode, limit: Duration) -> Self {
        Self { mode, limit, active_since: None, last_key_event: None }
    }

    pub fn is_active(&self) -> bool {
        self.active_since.is_some()
    }

    fn start(&mut self, now: Instant) -> Action {
        if self.active_since.is_some() {
            return Action::None;
        }
        self.active_since = Some(now);
        Action::Start
    }

    fn stop(&mut self, reason: StopReason) -> Action {
        if self.active_since.take().is_some() { Action::Stop(reason) } else { Action::None }
    }

    /// Terminal key pressed (not an auto-repeat event of a terminal that reports them).
    pub fn key_press(&mut self, now: Instant) -> Action {
        let repeat = self.last_key_event.is_some_and(|t| now.duration_since(t) < DEBOUNCE);
        self.last_key_event = Some(now);
        if repeat {
            return Action::None;
        }
        match self.mode {
            KeyMode::Hold => self.start(now),
            KeyMode::Toggle if self.is_active() => self.stop(StopReason::Released),
            KeyMode::Toggle => self.start(now),
        }
    }

    /// Terminal key released (only reported in hold mode).
    pub fn key_release(&mut self) -> Action {
        match self.mode {
            KeyMode::Hold => self.stop(StopReason::Released),
            KeyMode::Toggle => Action::None,
        }
    }

    /// Hardware PTT key (evdev): real press and release, independent of the terminal.
    pub fn hardware_down(&mut self, now: Instant) -> Action {
        self.start(now)
    }

    pub fn hardware_up(&mut self) -> Action {
        self.stop(StopReason::Released)
    }

    /// Call regularly; enforces the transmit limit.
    pub fn tick(&mut self, now: Instant) -> Action {
        match self.active_since {
            Some(t) if now.duration_since(t) >= self.limit => self.stop(StopReason::Limit),
            _ => Action::None,
        }
    }

    pub fn shutdown(&mut self) -> Action {
        self.stop(StopReason::Shutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(60);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn hold_mode_follows_press_and_release() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Hold, LIMIT);
        assert_eq!(p.key_press(t0), Action::Start);
        assert!(p.is_active());
        assert_eq!(p.key_release(), Action::Stop(StopReason::Released));
        assert!(!p.is_active());
        assert_eq!(p.key_release(), Action::None, "release without press is ignored");
    }

    #[test]
    fn toggle_mode_alternates_and_ignores_release() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Toggle, LIMIT);
        assert_eq!(p.key_press(t0), Action::Start);
        assert_eq!(p.key_release(), Action::None);
        assert!(p.is_active());
        assert_eq!(p.key_press(at(t0, 1000)), Action::Stop(StopReason::Released));
        assert_eq!(p.key_press(at(t0, 2000)), Action::Start);
    }

    #[test]
    fn repeated_presses_do_not_flip_toggle_mode() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Toggle, LIMIT);
        assert_eq!(p.key_press(t0), Action::Start);
        // held key: a burst of presses 30 ms apart
        for i in 1..=20 {
            assert_eq!(p.key_press(at(t0, i * 30)), Action::None, "repeat {i}");
        }
        assert!(p.is_active());
        // a deliberate tap later stops it
        assert_eq!(p.key_press(at(t0, 20 * 30 + 400)), Action::Stop(StopReason::Released));
    }

    #[test]
    fn hardware_key_is_hold_and_independent_of_mode() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Toggle, LIMIT);
        assert_eq!(p.hardware_down(t0), Action::Start);
        assert_eq!(p.hardware_down(at(t0, 10)), Action::None, "already transmitting");
        assert_eq!(p.hardware_up(), Action::Stop(StopReason::Released));
    }

    #[test]
    fn limit_releases_a_stuck_ptt_once() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Hold, LIMIT);
        p.key_press(t0);
        assert_eq!(p.tick(at(t0, 59_000)), Action::None);
        assert_eq!(p.tick(at(t0, 60_000)), Action::Stop(StopReason::Limit));
        assert_eq!(p.tick(at(t0, 61_000)), Action::None);
        assert!(!p.is_active());
    }

    #[test]
    fn shutdown_stops_only_if_active() {
        let t0 = Instant::now();
        let mut p = Ptt::new(KeyMode::Hold, LIMIT);
        assert_eq!(p.shutdown(), Action::None);
        p.key_press(t0);
        assert_eq!(p.shutdown(), Action::Stop(StopReason::Shutdown));
    }
}
