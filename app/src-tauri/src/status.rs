//! One-line status text (shown in the Android notification) derived from the server state.

use funk_core::state::ServerState;

/// Input to the status pump: what the audio side or the PTT button just did.
pub enum StatusInput {
    Speaking(Vec<u32>),
    Tx(bool),
}

pub fn status_line(st: &ServerState, speaking: &[u32], tx: bool) -> String {
    let channel = st
        .own_channel()
        .and_then(|id| st.channels.get(&id))
        .map(|c| if c.name.is_empty() { "Root" } else { c.name.as_str() })
        .unwrap_or("kein Kanal");
    if tx {
        return format!("{channel} · Du sendest");
    }
    let others: Vec<&str> = speaking
        .iter()
        .filter(|&&s| Some(s) != st.own_session)
        .map(|s| st.users.get(s).map_or("?", |u| u.name.as_str()))
        .collect();
    match others.len() {
        0 => format!("{channel} · Kanal frei"),
        1 => format!("{channel} · {} spricht", others[0]),
        _ => format!("{channel} · {} sprechen", others.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use funk_core::proto::{ChannelState, ServerSync, UserState};

    fn state() -> ServerState {
        let mut s = ServerState::default();
        s.apply_sync(&ServerSync { session: Some(1), ..Default::default() });
        s.apply_channel(&ChannelState { channel_id: Some(0), name: Some(String::new()), ..Default::default() });
        s.apply_channel(&ChannelState { channel_id: Some(5), name: Some("Kanal 5".into()), ..Default::default() });
        for (id, n, ch) in [(1, "ich", 5), (2, "Anna", 5), (3, "Ben", 5)] {
            s.apply_user(&UserState { session: Some(id), name: Some(n.into()), channel_id: Some(ch), ..Default::default() });
        }
        s
    }

    #[test]
    fn idle_speaking_and_tx() {
        let s = state();
        assert_eq!(status_line(&s, &[], false), "Kanal 5 · Kanal frei");
        assert_eq!(status_line(&s, &[2], false), "Kanal 5 · Anna spricht");
        assert_eq!(status_line(&s, &[2, 3], false), "Kanal 5 · Anna, Ben sprechen");
        assert_eq!(status_line(&s, &[1], false), "Kanal 5 · Kanal frei", "own voice is not 'someone speaking'");
        assert_eq!(status_line(&s, &[2], true), "Kanal 5 · Du sendest");
    }

    #[test]
    fn unknown_channel() {
        assert_eq!(status_line(&ServerState::default(), &[], false), "kein Kanal · Kanal frei");
    }
}
