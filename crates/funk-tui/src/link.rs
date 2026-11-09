//! Connection to the server plus audio (or the no-audio test mode), exposed as one handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use funk_audio::{Audio, DevicePrefs, Meter, Shared};
use funk_core::voice::{FRAME_SAMPLES, Mixer, SAMPLE_RATE, Transmitter, decode_packet};
use funk_core::{Client, Config, ConnectOptions, Event};

use crate::app::Msg;

pub struct LinkOpts {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub pin: Option<[u8; 32]>,
    pub no_audio: bool,
    pub prefs: DevicePrefs,
}

#[derive(Debug)]
pub enum LinkError {
    Connect(funk_core::Error),
    Audio(String),
}

pub struct Link {
    pub rt: tokio::runtime::Runtime,
    pub client: Client,
    shared: Arc<Shared>,
    _audio: Option<Audio>,
    stop: Arc<AtomicBool>,
    /// Devices in use, for the display ("MIC … · LS …").
    pub devices: String,
}

pub fn connect(opts: &LinkOpts, tx: Sender<Msg>) -> Result<Link, LinkError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| LinkError::Audio(format!("runtime: {e}")))?;
    let cfg = Config {
        host: opts.host.clone(),
        port: opts.port,
        username: opts.username.clone(),
        password: opts.password.clone().filter(|p| !p.is_empty()),
        tokens: vec![],
        pinned_sha256: opts.pin,
    };
    let (client, mut events) = rt
        .block_on(funk_core::connect_with(cfg, ConnectOptions { auto_reconnect: true, ..Default::default() }))
        .map_err(LinkError::Connect)?;
    let shared = Arc::new(Shared { tx: Mutex::new(Transmitter::new().map_err(|e| LinkError::Audio(e.to_string()))?), mixer: Mutex::new(Mixer::new()) });
    let stop = Arc::new(AtomicBool::new(false));

    // Network events -> UI messages, incoming voice -> mixer.
    {
        let (client, shared, tx) = (client.clone(), shared.clone(), tx.clone());
        let _ = tx.send(Msg::State(Box::new(client.state())));
        rt.spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    Event::ChannelUpdated(_) | Event::UserUpdated(_) | Event::UserRemoved(_) => {
                        let _ = tx.send(Msg::State(Box::new(client.state())));
                    }
                    Event::Voice(b) => {
                        if let Some(p) = decode_packet(&b) {
                            if let Ok(mut m) = shared.mixer.lock() {
                                let _ = m.push(p);
                            }
                        }
                    }
                    Event::Latency(ms) => {
                        let _ = tx.send(Msg::Latency(ms));
                    }
                    Event::PermissionDenied(r) => {
                        let _ = tx.send(Msg::Notice(r));
                    }
                    Event::Reconnecting { attempt, reason } => {
                        if attempt == 0 {
                            let _ = tx.send(Msg::Reconnecting(true));
                            let _ = tx.send(Msg::Notice(format!("VERBINDUNG VERLOREN: {}", reason.unwrap_or_default())));
                        }
                    }
                    Event::Reconnected => {
                        let _ = tx.send(Msg::Reconnecting(false));
                        let _ = tx.send(Msg::State(Box::new(client.state())));
                        let _ = tx.send(Msg::Notice("WIEDER VERBUNDEN".into()));
                    }
                    Event::Disconnected(r) => {
                        let _ = tx.send(Msg::Disconnected(r));
                        break;
                    }
                    Event::Connected { .. } | Event::Text { .. } | Event::PluginData { .. } => {}
                }
            }
        });
    }

    let (audio, devices) = if opts.no_audio {
        spawn_ticker(shared.clone(), client.clone(), tx.clone(), stop.clone());
        (None, "KEIN AUDIO (TESTMODUS: PTT SENDET TESTTON)".to_string())
    } else {
        let (c, t_speak, t_level) = (client.clone(), tx.clone(), tx.clone());
        let audio = Audio::start(
            shared.clone(),
            Arc::new(move |p| {
                c.try_send_voice(p);
            }),
            Arc::new(move |info| {
                let _ = t_speak.send(Msg::Speaking(info.speaking));
            }),
            Arc::new(move |l| {
                let _ = t_level.send(Msg::Level { tx: l.tx, bands: l.bands });
            }),
            opts.prefs.clone(),
        )
        .map_err(|e| LinkError::Audio(e.to_string()))?;
        let mut desc = format!("MIC {} · LS {}", audio.input_name, audio.output_name);
        if !audio.notes.is_empty() {
            desc.push_str(" (Ausweichgerät!)");
        }
        (Some(audio), desc)
    };
    Ok(Link { rt, client, shared, _audio: audio, stop, devices })
}

/// Without an audio device nothing pulls the mixer, so a 20 ms ticker does it: it reports who is
/// speaking and the spectrum of the decoded audio, and while PTT is active it sends a test tone.
fn spawn_ticker(shared: Arc<Shared>, client: Client, tx: Sender<Msg>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let meter = Meter::new(SAMPLE_RATE);
        let mut frame = [0i16; FRAME_SAMPLES];
        let mut last_speaking: Vec<u32> = Vec::new();
        let mut phase = 0usize;
        // Absolute schedule: sleeping a fixed 20 ms drifts slower than real time and the listener's
        // jitter buffer would run dry.
        let mut next = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            next += Duration::from_millis(20);
            std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
            let info = match shared.mixer.lock() {
                Ok(mut m) => m.mix(&mut frame),
                Err(_) => continue,
            };
            if info.speaking != last_speaking || !info.ended.is_empty() {
                last_speaking = info.speaking.clone();
                let _ = tx.send(Msg::Speaking(info.speaking.clone()));
            }
            if !info.speaking.is_empty() {
                let _ = tx.send(Msg::Level { tx: false, bands: meter.analyze(&frame) });
            }
            if let Ok(mut t) = shared.tx.lock() {
                if t.is_active() {
                    let tone: Vec<i16> = (0..FRAME_SAMPLES)
                        .map(|i| (((phase + i) as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 6000.0) as i16)
                        .collect();
                    phase += FRAME_SAMPLES;
                    let _ = tx.send(Msg::Level { tx: true, bands: meter.analyze(&tone) });
                    if let Ok(packets) = t.push_pcm(&tone) {
                        for p in packets {
                            client.try_send_voice(p);
                        }
                    }
                }
            }
        }
    });
}

impl Link {
    pub fn ptt_start(&self) {
        if let Ok(mut t) = self.shared.tx.lock() {
            if !t.is_active() {
                t.start();
            }
        }
    }

    /// Ends the transmission and sends the terminator packet the server's simplex logic waits for.
    pub fn ptt_stop(&self) {
        if let Ok(mut t) = self.shared.tx.lock() {
            if let Ok(Some(last)) = t.stop() {
                self.client.try_send_voice(last);
            }
        }
    }

    pub fn join(&self, channel: u32) {
        let c = self.client.clone();
        self.rt.spawn(async move {
            let _ = c.join_channel(channel).await;
        });
    }

    pub fn close(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.ptt_stop();
        // Let the writer task flush the terminator before the connection is dropped.
        std::thread::sleep(Duration::from_millis(150));
    }
}
