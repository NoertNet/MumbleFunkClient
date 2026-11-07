//! Mirror of the channel/user tree the server announces.

use std::collections::HashMap;

use crate::proto::{ChannelRemove, ChannelState, ServerSync, UserRemove, UserState};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Channel {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct User {
    pub session: u32,
    pub name: String,
    pub channel: u32,
    pub mute: bool,
    pub deaf: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ServerState {
    pub own_session: Option<u32>,
    pub welcome_text: String,
    pub max_bandwidth: Option<u32>,
    pub synced: bool,
    pub channels: HashMap<u32, Channel>,
    pub users: HashMap<u32, User>,
}

impl ServerState {
    pub fn own_channel(&self) -> Option<u32> {
        self.users.get(&self.own_session?).map(|u| u.channel)
    }

    pub fn users_in(&self, channel: u32) -> Vec<&User> {
        let mut v: Vec<_> = self.users.values().filter(|u| u.channel == channel).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn apply_sync(&mut self, m: &ServerSync) {
        self.own_session = m.session;
        self.max_bandwidth = m.max_bandwidth;
        self.welcome_text = m.welcome_text.clone().unwrap_or_default();
        self.synced = true;
    }

    pub fn apply_channel(&mut self, m: &ChannelState) -> Option<u32> {
        let id = m.channel_id?;
        let c = self.channels.entry(id).or_insert_with(|| Channel { id, ..Default::default() });
        if m.parent.is_some() {
            c.parent = m.parent;
        }
        if let Some(n) = &m.name {
            c.name = n.clone();
        }
        if let Some(d) = &m.description {
            c.description = d.clone();
        }
        Some(id)
    }

    pub fn remove_channel(&mut self, m: &ChannelRemove) {
        self.channels.remove(&m.channel_id);
    }

    pub fn apply_user(&mut self, m: &UserState) -> Option<u32> {
        let id = m.session?;
        let u = self.users.entry(id).or_insert_with(|| User { session: id, ..Default::default() });
        if let Some(n) = &m.name {
            u.name = n.clone();
        }
        if let Some(c) = m.channel_id {
            u.channel = c;
        }
        if let Some(v) = m.mute {
            u.mute = v;
        }
        if let Some(v) = m.deaf {
            u.deaf = v;
        }
        if let Some(v) = m.self_mute {
            u.self_mute = v;
        }
        if let Some(v) = m.self_deaf {
            u.self_deaf = v;
        }
        Some(id)
    }

    pub fn remove_user(&mut self, m: &UserRemove) {
        self.users.remove(&m.session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_move_updates_channel() {
        let mut s = ServerState::default();
        s.apply_sync(&ServerSync { session: Some(7), ..Default::default() });
        s.apply_user(&UserState { session: Some(7), name: Some("a".into()), channel_id: Some(0), ..Default::default() });
        s.apply_user(&UserState { session: Some(7), channel_id: Some(3), ..Default::default() });
        assert_eq!(s.own_channel(), Some(3));
        assert_eq!(s.users[&7].name, "a");
    }
}
