//! One connected session: protocol client, audio streams and the event pump to the UI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use funk_audio::{Audio, DevicePrefs, Level, Shared};
use funk_core::voice::{Mixer, Transmitter, decode_packet};
use funk_core::{Client, Config, ConnectOptions, Error, Event};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::ConnectArgs;
use crate::settings::{self, Profile, hex, parse_pin};
use crate::status::{StatusInput, status_line};
use crate::android;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

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

/// Which devices the audio streams ended up on (and why a chosen one was skipped).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AudioDto {
    pub input: String,
    pub output: String,
    pub notes: Vec<String>,
}

#[derive(Serialize, Clone)]
struct LevelDto {
    tx: bool,
    bands: Vec<u8>,
}

pub struct Session {
    client: Client,
    shared: Arc<Shared>,
    _audio_stop: mpsc::Sender<()>,
    pump: tauri::async_runtime::JoinHandle<()>,
    status_tx: UnboundedSender<StatusInput>,
    pub audio: AudioDto,
    /// True while the connection is lost and being re-established.
    reconnecting: Arc<AtomicBool>,
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

        // Remember the server before connecting, so a failed attempt (wrong network, permission
        // missing, ...) never forces the user to type everything again.
        stored.remember(Profile {
            host: args.host.clone(),
            port: args.port,
            username: args.username.clone(),
            password: if args.remember_password { args.password.clone().filter(|p| !p.is_empty()) } else { None },
        });
        settings::save(&app, &stored);

        let cfg = Config {
            host: args.host.clone(),
            port: args.port,
            username: args.username.clone(),
            password: args.password.clone().filter(|p| !p.is_empty()),
            tokens: vec![],
            pinned_sha256: pin,
        };
        // Reconnect automatically; ping less often while the window is not visible (still well inside the
        // server's 30 s silence limit and the UI's 13 s "no signal" threshold).
        let opts = ConnectOptions { auto_reconnect: true, is_background: Arc::new(android::is_background), ..Default::default() };
        let (client, mut events) = funk_core::connect_with(cfg, opts).await.map_err(|e| match e {
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

        // Certificate pin: trust on first use, replace only when the user accepted a new one.
        stored.pins.entry(key.clone()).and_modify(|p| {
            if args.trust_new_cert {
                *p = hex(&client.cert_sha256());
            }
        }).or_insert_with(|| hex(&client.cert_sha256()));
        settings::save(&app, &stored);

        let (status_tx, mut status_rx) = unbounded_channel::<StatusInput>();
        let (levels_tx, mut levels_rx) = unbounded_channel::<Level>();
        // Device choice only exists on the desktop; phones always use the system routing.
        let prefs = if cfg!(target_os = "android") {
            DevicePrefs::default()
        } else {
            DevicePrefs { input: stored.input_device.clone(), output: stored.output_device.clone() }
        };
        let shared = Arc::new(Shared { tx: Mutex::new(Transmitter::new().map_err(|e| e.to_string())?), mixer: Mutex::new(Mixer::new()) });

        // Audio streams live on their own thread (cpal streams are not Send on every platform).
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<AudioDto, String>>();
        {
            let shared = shared.clone();
            let c = client.clone();
            let a = app.clone();
            let st = status_tx.clone();
            let lt = levels_tx.clone();
            std::thread::spawn(move || {
                let started = Audio::start(
                    shared,
                    Arc::new(move |p| {
                        c.try_send_voice(p);
                    }),
                    Arc::new(move |info| {
                        let _ = a.emit("speaking", info.speaking.clone());
                        // Unbounded + non-blocking: this runs on the real-time audio thread.
                        let _ = st.send(StatusInput::Speaking(info.speaking));
                    }),
                    Arc::new(move |level| {
                        let _ = lt.send(level);
                    }),
                    prefs,
                );
                match started {
                    Ok(audio) => {
                        let _ = ready_tx.send(Ok(AudioDto {
                            input: audio.input_name.clone(),
                            output: audio.output_name.clone(),
                            notes: audio.notes.clone(),
                        }));
                        let _ = stop_rx.recv(); // returns when the Session is dropped
                        drop(audio);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("Audio: {e}")));
                    }
                }
            });
        }
        let audio_dto = tauri::async_runtime::spawn_blocking(move || ready_rx.recv().unwrap_or_else(|_| Err("audio thread died".into())))
            .await
            .map_err(|e| e.to_string())??;

        // Spectrum for the equalizer: the audio threads only push into the channel (no IPC there).
        {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                while let Some(l) = levels_rx.recv().await {
                    let _ = app.emit("levels", LevelDto { tx: l.tx, bands: l.bands.to_vec() });
                }
            });
        }

        let reconnecting = Arc::new(AtomicBool::new(false));
        let pump = {
            let (app, shared, client, reconnecting) = (app.clone(), shared.clone(), client.clone(), reconnecting.clone());
            tauri::async_runtime::spawn(async move {
                let (mut speaking, mut tx_on, mut last_status) = (Vec::<u32>::new(), false, String::new());
                let mut lost = false;
                // Updates the Android notification (no-op elsewhere); only calls out when text changed.
                // While the connection is lost the notification keeps saying so (and the foreground
                // service stays up, otherwise it could not be restarted from the background).
                let mut refresh = |client: &Client, speaking: &[u32], tx_on: bool, lost: bool| {
                    let line = if lost { "Verbindung verloren – verbinde neu …".to_string() } else { status_line(&client.state(), speaking, tx_on) };
                    if line != last_status {
                        android::update(&line, true);
                        last_status = line;
                    }
                };
                refresh(&client, &speaking, tx_on, lost);
                loop {
                    tokio::select! {
                        Some(input) = status_rx.recv() => {
                            match input {
                                StatusInput::Speaking(s) => speaking = s,
                                StatusInput::Tx(t) => tx_on = t,
                            }
                            refresh(&client, &speaking, tx_on, lost);
                        }
                        ev = events.recv() => {
                            let Some(ev) = ev else { break };
                            match ev {
                                Event::Connected { .. } => {}
                                Event::ChannelUpdated(_) | Event::UserUpdated(_) | Event::UserRemoved(_) => {
                                    let _ = app.emit("state", dto_of(&client));
                                    refresh(&client, &speaking, tx_on, lost);
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
                                Event::Latency(ms) => {
                                    let _ = app.emit("latency", ms);
                                }
                                Event::PermissionDenied(r) => {
                                    let _ = app.emit("notice", r);
                                }
                                Event::Reconnecting { attempt, reason } => {
                                    if attempt == 0 {
                                        // Whatever was being transmitted is over; people who spoke are gone with the
                                        // old session. A new PTT press is needed after the reconnect.
                                        reconnecting.store(true, Ordering::SeqCst);
                                        lost = true;
                                        speaking.clear();
                                        crate::reset_ptt(&app);
                                        if let Ok(mut m) = shared.mixer.lock() {
                                            *m = Mixer::new();
                                        }
                                        android::log(&format!("Verbindung verloren: {}", reason.as_deref().unwrap_or("-")));
                                        let _ = app.emit("reconnecting", serde_json::json!({ "attempt": attempt, "reason": reason }));
                                    } else if attempt == 1 || attempt % 5 == 0 {
                                        android::log(&format!("Wiederverbindung Versuch {attempt}: {}", reason.as_deref().unwrap_or("-")));
                                    }
                                    refresh(&client, &speaking, tx_on, lost);
                                }
                                Event::Reconnected => {
                                    reconnecting.store(false, Ordering::SeqCst);
                                    lost = false;
                                    android::log("Wieder verbunden");
                                    let _ = app.emit("reconnected", ());
                                    let _ = app.emit("state", dto_of(&client));
                                    refresh(&client, &speaking, tx_on, lost);
                                }
                                Event::Disconnected(reason) => {
                                    android::log(&format!("Verbindung beendet: {}", reason.as_deref().unwrap_or("-")));
                                    reconnecting.store(false, Ordering::SeqCst);
                                    let _ = app.emit("disconnected", reason);
                                    break;
                                }
                            }
                        }
                    }
                }
                android::update("", false);
            })
        };

        android::log(&format!("Verbunden mit {}:{}", args.host, args.port));
        Ok(Session { client, shared, _audio_stop: stop_tx, pump, status_tx, audio: audio_dto, reconnecting })
    }

    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting.load(Ordering::SeqCst)
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    pub fn dto(&self) -> StateDto {
        dto_of(&self.client)
    }

    pub fn ptt(&self, down: bool) {
        let _ = self.status_tx.send(StatusInput::Tx(down));
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
        android::update("", false);
    }
}
