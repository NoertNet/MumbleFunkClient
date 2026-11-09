//! Application state of the terminal client and the messages that change it.

use std::time::{Duration, Instant};

use funk_core::state::{Channel, ServerState};

pub use funk_audio::BANDS;

/// Pings come every 4 s; without an answer for this long the link counts as "no signal".
const SIGNAL_STALE: Duration = Duration::from_secs(13);
/// The spectrum display falls back to zero when no new levels arrive for this long.
const EQ_IDLE: Duration = Duration::from_millis(150);
const NOTICE_TTL: Duration = Duration::from_secs(6);

/// Everything that flows into the UI loop from the network and audio threads.
pub enum Msg {
    State(Box<ServerState>),
    Speaking(Vec<u32>),
    Level { tx: bool, bands: [u8; BANDS] },
    Latency(u32),
    Notice(String),
    /// Hardware PTT key (evdev): true = pressed.
    HardwarePtt(bool),
    /// The connection was lost (true) / is back (false). Transmitting must stop when it is lost.
    Reconnecting(bool),
    Disconnected(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Tx,
    Rx,
    /// We press PTT while someone else is speaking (simplex: the server rejects us).
    Busy,
    /// Connection lost, being re-established.
    Lost,
}

pub struct App {
    pub server: String,
    pub username: String,
    pub st: ServerState,
    pub speaking: Vec<u32>,
    pub tx: bool,
    pub latency: Option<(u32, Instant)>,
    pub eq_target: [f32; BANDS],
    pub eq_cur: [f32; BANDS],
    eq_last: Option<Instant>,
    notice: Option<(String, Instant)>,
    /// How PTT is operated, shown in the PTT box ("HALTEN (Leertaste)", ...).
    pub ptt_info: String,
    /// Audio devices in use.
    pub devices: String,
    pub list_open: bool,
    pub list_sel: usize,
    pub reconnecting: bool,
    /// Set when the connection ended: `Some(reason)`.
    pub disconnected: Option<Option<String>>,
}

impl App {
    pub fn new(server: String, username: String, ptt_info: String, devices: String) -> Self {
        Self {
            server,
            username,
            st: ServerState::default(),
            speaking: Vec::new(),
            tx: false,
            latency: None,
            eq_target: [0.0; BANDS],
            eq_cur: [0.0; BANDS],
            eq_last: None,
            notice: None,
            ptt_info,
            devices,
            list_open: false,
            list_sel: 0,
            reconnecting: false,
            disconnected: None,
        }
    }

    pub fn others_speaking(&self) -> Vec<&str> {
        self.speaking
            .iter()
            .filter(|&&s| Some(s) != self.st.own_session)
            .map(|s| self.st.users.get(s).map_or("?", |u| u.name.as_str()))
            .collect()
    }

    pub fn mode(&self) -> Mode {
        if self.reconnecting {
            return Mode::Lost;
        }
        let rx = !self.others_speaking().is_empty();
        match (self.tx, rx) {
            (true, true) => Mode::Busy,
            (true, false) => Mode::Tx,
            (false, true) => Mode::Rx,
            (false, false) => Mode::Idle,
        }
    }

    pub fn channels_sorted(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = self.st.channels.values().collect();
        v.sort_by_key(|c| c.id);
        v
    }

    /// (1-based position, total) of the channel we are in.
    pub fn channel_position(&self) -> Option<(usize, usize)> {
        let list = self.channels_sorted();
        let own = self.st.own_channel()?;
        list.iter().position(|c| c.id == own).map(|i| (i + 1, list.len()))
    }

    pub fn own_channel_name(&self) -> String {
        self.st
            .own_channel()
            .and_then(|id| self.st.channels.get(&id))
            .map(|c| if c.name.is_empty() { "Root".to_string() } else { c.name.clone() })
            .unwrap_or_else(|| "kein Kanal".to_string())
    }

    /// Channel id reached by stepping `dir` (+1/-1) from the current one, wrapping around.
    pub fn step_channel(&self, dir: i32) -> Option<u32> {
        let list = self.channels_sorted();
        if list.len() < 2 {
            return None;
        }
        let own = self.st.own_channel()?;
        let i = list.iter().position(|c| c.id == own)? as i32;
        Some(list[(i + dir).rem_euclid(list.len() as i32) as usize].id)
    }

    /// Signal bars 0..=4 from the ping time; `None` ms = never measured.
    pub fn signal(&self, now: Instant) -> (u8, Option<u32>, bool) {
        match self.latency {
            None => (0, None, false),
            Some((ms, at)) if now.duration_since(at) > SIGNAL_STALE => (0, Some(ms), true),
            Some((ms, _)) => {
                let level = match ms {
                    0..=49 => 4,
                    50..=99 => 3,
                    100..=199 => 2,
                    200..=499 => 1,
                    _ => 0,
                };
                (level, Some(ms), false)
            }
        }
    }

    pub fn notice(&self, now: Instant) -> Option<&str> {
        self.notice.as_ref().filter(|(_, at)| now.duration_since(*at) < NOTICE_TTL).map(|(t, _)| t.as_str())
    }

    pub fn set_notice(&mut self, text: impl Into<String>, now: Instant) {
        self.notice = Some((text.into(), now));
    }

    pub fn apply(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::State(st) => self.st = *st,
            Msg::Speaking(s) => self.speaking = s,
            Msg::Level { tx, bands } => {
                // Own microphone levels only count while we transmit, channel levels always.
                if !tx || self.tx {
                    for (t, b) in self.eq_target.iter_mut().zip(bands) {
                        *t = b as f32 / 255.0;
                    }
                    self.eq_last = Some(now);
                }
            }
            Msg::Latency(ms) => self.latency = Some((ms, now)),
            Msg::Notice(t) => self.set_notice(t, now),
            Msg::Reconnecting(lost) => {
                self.reconnecting = lost;
                if lost {
                    self.speaking.clear();
                    self.latency = None;
                }
            }
            Msg::Disconnected(r) => self.disconnected = Some(r),
            Msg::HardwarePtt(_) => {} // handled by the main loop, which owns the PTT state machine
        }
    }

    /// Advance the spectrum animation by one frame. Returns true while anything is still moving.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.eq_last.is_none_or(|t| now.duration_since(t) > EQ_IDLE) {
            self.eq_target = [0.0; BANDS];
        }
        let mut moving = false;
        for (c, &t) in self.eq_cur.iter_mut().zip(&self.eq_target) {
            // fast attack, slower release: like a real analyser
            let mut next = if t > *c { *c + (t - *c) * 0.65 } else { *c + (t - *c) * 0.16 };
            if t == 0.0 && next < 0.015 {
                next = 0.0;
            }
            *c = next;
            moving |= next > 0.0 || t > 0.0;
        }
        moving
    }
}

#[cfg(test)]
pub fn sample_app() -> App {
    use funk_core::proto::{ChannelState, ServerSync, UserState};
    let mut s = ServerState::default();
    s.apply_sync(&ServerSync { session: Some(1), ..Default::default() });
    s.apply_channel(&ChannelState { channel_id: Some(0), name: Some(String::new()), ..Default::default() });
    s.apply_channel(&ChannelState { channel_id: Some(5), name: Some("Einsatz Nord".into()), ..Default::default() });
    s.apply_channel(&ChannelState { channel_id: Some(7), name: Some("Kanal 7".into()), ..Default::default() });
    for (id, n, ch) in [(1, "Alpha1", 5), (2, "Bravo2", 5), (3, "Charlie3", 7)] {
        s.apply_user(&UserState { session: Some(id), name: Some(n.into()), channel_id: Some(ch), ..Default::default() });
    }
    let mut a = App::new("funk.example.org".into(), "Alpha1".into(), "HALTEN (Leertaste)".into(), "MIC USB · LS HDMI".into());
    a.st = s;
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use funk_core::proto::UserState;

    fn app() -> App {
        sample_app()
    }

    #[test]
    fn modes() {
        let mut a = app();
        assert_eq!(a.mode(), Mode::Idle);
        a.speaking = vec![2];
        assert_eq!(a.mode(), Mode::Rx);
        a.tx = true;
        assert_eq!(a.mode(), Mode::Busy);
        a.speaking = vec![1];
        assert_eq!(a.mode(), Mode::Tx, "own voice is not 'someone else'");
    }

    #[test]
    fn lost_connection_overrides_everything_and_clears_speakers() {
        let mut a = app();
        a.speaking = vec![2];
        a.tx = true;
        a.apply(Msg::Reconnecting(true), Instant::now());
        assert_eq!(a.mode(), Mode::Lost);
        assert!(a.speaking.is_empty());
        a.apply(Msg::Reconnecting(false), Instant::now());
        assert_eq!(a.mode(), Mode::Tx, "back to normal; tx is cleared by the main loop, not by the state");
    }

    #[test]
    fn channel_stepping_wraps() {
        let a = app();
        assert_eq!(a.channel_position(), Some((2, 3)));
        assert_eq!(a.step_channel(1), Some(7));
        assert_eq!(a.step_channel(-1), Some(0));
        let mut b = app();
        b.st.apply_user(&UserState { session: Some(1), channel_id: Some(7), ..Default::default() });
        assert_eq!(b.step_channel(1), Some(0), "wraps past the last channel");
    }

    #[test]
    fn signal_levels_and_staleness() {
        let mut a = app();
        let t0 = Instant::now();
        assert_eq!(a.signal(t0), (0, None, false));
        for (ms, want) in [(10, 4), (60, 3), (150, 2), (300, 1), (900, 0)] {
            a.apply(Msg::Latency(ms), t0);
            assert_eq!(a.signal(t0).0, want, "{ms} ms");
        }
        a.apply(Msg::Latency(20), t0);
        assert_eq!(a.signal(t0 + Duration::from_secs(14)), (0, Some(20), true));
    }

    #[test]
    fn equalizer_follows_levels_and_decays() {
        let mut a = app();
        let t0 = Instant::now();
        a.apply(Msg::Level { tx: false, bands: [255; BANDS] }, t0);
        assert!(a.tick(t0));
        assert!(a.eq_cur[0] > 0.5);
        // no more levels: falls to zero and reports "not moving" eventually
        let mut t = t0;
        let mut moving = true;
        for _ in 0..80 {
            t += Duration::from_millis(40);
            moving = a.tick(t);
        }
        assert!(!moving);
        assert!(a.eq_cur.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn own_microphone_levels_only_while_transmitting() {
        let mut a = app();
        let t0 = Instant::now();
        a.apply(Msg::Level { tx: true, bands: [255; BANDS] }, t0);
        assert_eq!(a.eq_target[0], 0.0);
        a.tx = true;
        a.apply(Msg::Level { tx: true, bands: [255; BANDS] }, t0);
        assert_eq!(a.eq_target[0], 1.0);
    }

    #[test]
    fn notices_expire() {
        let mut a = app();
        let t0 = Instant::now();
        a.set_notice("hallo", t0);
        assert_eq!(a.notice(t0), Some("hallo"));
        assert_eq!(a.notice(t0 + Duration::from_secs(7)), None);
    }
}
