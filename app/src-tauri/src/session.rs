//! One connected session: protocol client, audio streams and the event pump to the UI.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use funk_audio::{Audio, Shared};
use funk_core::voice::{Mixer, Transmitter, decode_packet};
use funk_core::{Client, Config, Error, Event};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::ConnectArgs;
use crate::settings::{self, Profile, hex, parse_pin};

#[derive(Serialize, Clone)]
pub struct UserDto {
    session: u32,
    name: String,
}

#[derive(Serialize, Clone)]
pub struct ChannelDto {
    id: u32,
    parent: Option<u32>,
    name: String,
    users: Vec<UserDto>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StateDto {
    own_session: Option<u32>,
    own_channel: Option<u32>,
    welcome: String,
    channels: Vec<ChannelDto>,
}

/// Android 17+ blocks connections to the home network unless the user allows it.
#[cfg(target_os = "android")]
const ANDROID_HINT: &str =
    " Android: Prüfe in den App-Einstellungen unter Berechtigungen, ob „Geräte im lokalen Netzwerk“ erlaubt ist.";
#[cfg(not(target_os = "android"))]
const ANDROID_HINT: &str = "";

pub struct Session {
    client: Client,
    shared: Arc<Shared>,
    _audio_stop: mpsc::Sender<()>,
    pump: tauri::async_runtime::JoinHandle<()>,
}

fn dto_of(client: &Client) -> StateDto {
    let st = client.state();
    let mut channels: Vec<ChannelDto> = st
        .channels
        .values()
        .map(|c| ChannelDto {
            id: c.id,
            parent: c.parent,
            name: c.name.clone(),
            users: st.users_in(c.id).into_iter().map(|u| UserDto { session: u.session, name: u.name.clone() }).collect(),
        })
        .collect();
    channels.sort_by_key(|c| c.id);
    StateDto { own_session: st.own_session, own_channel: st.own_channel(), welcome: st.welcome_text.clone(), channels }
}

impl Session {
    pub async fn connect(app: AppHandle, args: ConnectArgs) -> Result<Session, String> {
        let key = format!("{}:{}", args.host, args.port);
        let mut stored = settings::load(&app);
        let pin = if args.trust_new_cert { None } else { stored.pins.get(&key).and_then(|h| parse_pin(h)) };

        let cfg = Config {
            host: args.host.clone(),
            port: args.port,
            username: args.username.clone(),
            password: args.password.clone().filter(|p| !p.is_empty()),
            tokens: vec![],
            pinned_sha256: pin,
        };
        let (client, mut events) = funk_core::connect(cfg).await.map_err(|e| match e {
            Error::PinMismatch => "PIN_MISMATCH".to_string(),
            Error::Rejected(r) => format!("Server hat die Anmeldung abgelehnt: {r}"),
            Error::Dns(m) => format!("Server-Adresse nicht gefunden ({m}). Stimmt der Servername?"),
            Error::Unreachable(d) => format!(
                "Server nicht erreichbar: {d}. Prüfe Adresse, Port und Firewall, und ob dieses Gerät im richtigen Netz ist (WLAN/Mobilfunk).{}",
                ANDROID_HINT
            ),
            Error::Timeout(stage) => format!("Zeitüberschreitung bei: {stage}. Der Server antwortet nicht rechtzeitig.{ANDROID_HINT}"),
            e => e.to_string(),
        })?;

        // Remember the server as soon as the login worked, even if audio fails afterwards.
        stored.pins.entry(key.clone()).and_modify(|p| {
            if args.trust_new_cert {
                *p = hex(&client.cert_sha256());
            }
        }).or_insert_with(|| hex(&client.cert_sha256()));
        stored.remember(Profile {
            host: args.host.clone(),
            port: args.port,
            username: args.username.clone(),
            password: if args.remember_password { args.password.clone().filter(|p| !p.is_empty()) } else { None },
        });
        settings::save(&app, &stored);

        let shared = Arc::new(Shared { tx: Mutex::new(Transmitter::new().map_err(|e| e.to_string())?), mixer: Mutex::new(Mixer::new()) });

        // Audio streams live on their own thread (cpal streams are not Send on every platform).
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        {
            let shared = shared.clone();
            let c = client.clone();
            let a = app.clone();
            std::thread::spawn(move || {
                let started = Audio::start(
                    shared,
                    Arc::new(move |p| {
                        c.try_send_voice(p);
                    }),
                    Arc::new(move |info| {
                        let _ = a.emit("speaking", info.speaking);
                    }),
                );
                match started {
                    Ok(audio) => {
                        let _ = ready_tx.send(Ok(()));
                        let _ = stop_rx.recv(); // returns when the Session is dropped
                        drop(audio);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("Audio: {e}")));
                    }
                }
            });
        }
        tauri::async_runtime::spawn_blocking(move || ready_rx.recv().unwrap_or_else(|_| Err("audio thread died".into())))
            .await
            .map_err(|e| e.to_string())??;

        let pump = {
            let (app, shared, client) = (app.clone(), shared.clone(), client.clone());
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = events.recv().await {
                    match ev {
                        Event::Connected { .. } => {}
                        Event::ChannelUpdated(_) | Event::UserUpdated(_) | Event::UserRemoved(_) => {
                            let _ = app.emit("state", dto_of(&client));
                        }
                        Event::Voice(b) => {
                            if let Some(p) = decode_packet(&b) {
                                if let Ok(mut m) = shared.mixer.lock() {
                                    let _ = m.push(p);
                                }
                            }
                        }
                        Event::PluginData { sender, data_id, data } => {
                            let _ = app.emit(
                                "plugin",
                                serde_json::json!({ "sender": sender, "dataId": data_id, "data": String::from_utf8_lossy(&data) }),
                            );
                        }
                        Event::Text { actor, text } => {
                            let _ = app.emit("text", serde_json::json!({ "actor": actor, "text": text }));
                        }
                        Event::PermissionDenied(r) => {
                            let _ = app.emit("notice", r);
                        }
                        Event::Disconnected(reason) => {
                            let _ = app.emit("disconnected", reason);
                            break;
                        }
                    }
                }
            })
        };

        Ok(Session { client, shared, _audio_stop: stop_tx, pump })
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    pub fn dto(&self) -> StateDto {
        dto_of(&self.client)
    }

    pub fn ptt(&self, down: bool) {
        let mut tx = self.shared.tx.lock().unwrap();
        if down {
            if !tx.is_active() {
                tx.start();
            }
        } else if let Ok(Some(last)) = tx.stop() {
            // The terminator must go out; it is tiny, so the queue has room unless the link is dead.
            self.client.try_send_voice(last);
        }
    }

    pub fn close(self) {
        // Release PTT cleanly, then dropping `self` stops audio and closes the connection.
        self.ptt(false);
        self.pump.abort();
    }
}
