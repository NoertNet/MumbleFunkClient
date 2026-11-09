//! TLS connection, handshake and the read/write tasks of one server session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use prost::Message;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_rustls::TlsConnector;

use crate::proto::{self, Authenticate, Version};
use crate::state::ServerState;
use crate::wire::{MessageType, WireError, read_frame, write_frame};

const DNS_TIMEOUT: Duration = Duration::from_secs(8);
const TCP_TIMEOUT: Duration = Duration::from_secs(8);
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Protocol version we announce (Mumble 1.5.0).
const VERSION_V1: u32 = (1 << 16) | (5 << 8);
const VERSION_V2: u64 = (1 << 48) | (5 << 32);

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub tokens: Vec<String>,
    /// SHA-256 of the server certificate (DER). `None` accepts any certificate
    /// (trust on first use is up to the caller: read it from [`Event::Connected`]).
    pub pinned_sha256: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire: {0}")]
    Wire(#[from] WireError),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("invalid server name")]
    ServerName,
    #[error("server certificate does not match pinned fingerprint")]
    PinMismatch,
    #[error("rejected by server: {0}")]
    Rejected(String),
    /// The server still holds our previous session (it notices a dead link only after its own
    /// timeout). Temporary: worth retrying.
    #[error("user name already in use: {0}")]
    UsernameInUse(String),
    /// Temporary as well.
    #[error("server full: {0}")]
    ServerFull(String),
    #[error("connection closed")]
    Closed,
    #[error("could not resolve server address: {0}")]
    Dns(String),
    /// TCP connect failed for every address the name resolved to; lists each attempt.
    #[error("server unreachable: {0}")]
    Unreachable(String),
    #[error("timeout during {0}")]
    Timeout(&'static str),
}

#[derive(Debug)]
pub enum Event {
    /// Handshake finished; carries the server certificate fingerprint.
    Connected { session: u32, cert_sha256: [u8; 32] },
    ChannelUpdated(u32),
    UserUpdated(u32),
    UserRemoved(u32),
    Text { actor: Option<u32>, text: String },
    /// Raw voice packet tunnelled through TCP (decoded by the voice layer).
    Voice(Bytes),
    /// `PluginDataTransmission` from the server or another user.
    PluginData { sender: Option<u32>, data_id: String, data: Bytes },
    PermissionDenied(String),
    /// Round-trip time to the server in milliseconds, from the latest ping reply.
    Latency(u32),
    /// The connection was lost and is being re-established (only with `auto_reconnect`). Sent again
    /// for every failed attempt. Whatever the user was doing (PTT!) must be treated as ended.
    Reconnecting { attempt: u32, reason: Option<String> },
    /// Connected again: channels and users were reloaded and the previous channel re-joined.
    Reconnected,
    /// The connection is gone for good (without `auto_reconnect`: on any loss; with it: only after a
    /// fatal error such as a wrong password).
    Disconnected(Option<String>),
}

/// Tuning of a session. `ConnectOptions::default()` gives the plain behaviour of [`connect`].
#[derive(Clone)]
pub struct ConnectOptions {
    /// Re-establish the connection automatically after a loss.
    pub auto_reconnect: bool,
    /// Asked once per ping. While true (app not visible) pings are sent less often. Unknown or wrong
    /// answers must never lengthen the interval, so the default is `false` = foreground.
    pub is_background: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Ping interval while visible; also drives the latency readout.
    pub ping_foreground: Duration,
    /// Ping interval in the background. Must stay well below the server's 30 s silence timeout and
    /// below what the UI treats as "no signal" (13 s).
    pub ping_background: Duration,
    /// The link counts as dead after `2 × interval + dead_grace` without any incoming data.
    pub dead_grace: Duration,
    /// Waits between reconnect attempts; the last entry repeats.
    pub backoff: Vec<Duration>,
    /// A connection that the server closes cleanly within this time after it was established counts
    /// as a "quick drop". The server ends the *older* session when the same user name logs in again,
    /// so two devices with one name would otherwise kick each other forever.
    pub quick_drop_window: Duration,
    /// After this many quick drops in a row the session gives up and reports why. The other device
    /// is then no longer kicked and stays connected.
    pub max_quick_drops: u32,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            auto_reconnect: false,
            is_background: Arc::new(|| false),
            ping_foreground: Duration::from_secs(4),
            ping_background: Duration::from_secs(8),
            dead_grace: Duration::from_secs(10),
            backoff: [1, 2, 4, 8, 15].map(Duration::from_secs).to_vec(),
            quick_drop_window: Duration::from_secs(30),
            max_quick_drops: 2,
        }
    }
}

impl ConnectOptions {
    fn ping_interval(&self) -> Duration {
        if (self.is_background)() { self.ping_background } else { self.ping_foreground }
    }

    fn dead_after(&self) -> Duration {
        self.ping_interval() * 2 + self.dead_grace
    }

    fn wait_before_attempt(&self, attempt: usize) -> Duration {
        self.backoff.get(attempt).or(self.backoff.last()).copied().unwrap_or(Duration::from_secs(1))
    }
}

/// Errors worth another attempt; everything else (wrong password, pinned certificate changed, ...)
/// ends the session.
fn retryable(e: &Error) -> bool {
    !matches!(e, Error::PinMismatch | Error::Rejected(_) | Error::ServerName)
}

type Outgoing = (u16, Vec<u8>);

struct Inner {
    /// Sender of the *current* connection; swapped on reconnect, so every `Client` clone follows.
    tx: RwLock<mpsc::Sender<Outgoing>>,
    state: Arc<RwLock<ServerState>>,
    cert_sha256: RwLock<[u8; 32]>,
    closed: AtomicBool,
    ping_now: Arc<Notify>,
}

/// Cheap, cloneable handle to a running session.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    fn sender(&self) -> mpsc::Sender<Outgoing> {
        self.inner.tx.read().unwrap().clone()
    }

    /// SHA-256 of the server certificate seen during the (latest) TLS handshake.
    pub fn cert_sha256(&self) -> [u8; 32] {
        *self.inner.cert_sha256.read().unwrap()
    }

    pub fn state(&self) -> ServerState {
        self.inner.state.read().unwrap().clone()
    }

    async fn send<M: Message>(&self, ty: MessageType, m: &M) -> Result<(), Error> {
        self.sender().send((ty as u16, m.encode_to_vec())).await.map_err(|_| Error::Closed)
    }

    pub async fn join_channel(&self, channel_id: u32) -> Result<(), Error> {
        let session = self.inner.state.read().unwrap().own_session;
        let m = proto::UserState { session, channel_id: Some(channel_id), ..Default::default() };
        self.send(MessageType::UserState, &m).await
    }

    pub async fn send_text(&self, channel_id: u32, text: &str) -> Result<(), Error> {
        let m = proto::TextMessage { channel_id: vec![channel_id], message: text.into(), ..Default::default() };
        self.send(MessageType::TextMessage, &m).await
    }

    /// Non-blocking variant for audio callbacks. Returns false if the queue is full or closed
    /// (the packet is dropped, which is the right thing for real-time voice).
    pub fn try_send_voice(&self, packet: Vec<u8>) -> bool {
        self.sender().try_send((MessageType::UdpTunnel as u16, packet)).is_ok()
    }

    /// Send an already framed voice packet through the TCP tunnel.
    pub async fn send_voice(&self, packet: Vec<u8>) -> Result<(), Error> {
        self.sender().send((MessageType::UdpTunnel as u16, packet)).await.map_err(|_| Error::Closed)
    }

    /// Ping right now instead of waiting for the next interval (e.g. when the app comes to the
    /// foreground, so the latency readout is fresh immediately).
    pub fn ping_now(&self) {
        self.inner.ping_now.notify_one();
    }

    /// End the session for good: closes the connection and stops any reconnecting.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        let (dead, _) = mpsc::channel(1);
        *self.inner.tx.write().unwrap() = dead; // drops the live sender: the writer shuts the TLS session down
    }
}

#[derive(Debug)]
struct Verifier {
    pin: Option<[u8; 32]>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sha256(der: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, der);
    d.as_ref().try_into().unwrap()
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.pin {
            Some(p) if p != sha256(end_entity) => {
                Err(rustls::Error::General("certificate pin mismatch".into()))
            }
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, s: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, s, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, s: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, s, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Resolve `host` and connect to the first address that answers. Every step has a timeout and the
/// error names what failed, because "nothing happens" is the worst possible failure mode on a phone.
async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, Error> {
    let addrs: Vec<std::net::SocketAddr> = match tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(it)) => it.collect(),
        Ok(Err(e)) => return Err(Error::Dns(format!("{host}: {e}"))),
        Err(_) => return Err(Error::Timeout("address lookup")),
    };
    if addrs.is_empty() {
        return Err(Error::Dns(format!("{host}: no addresses")));
    }
    let mut failures = Vec::new();
    for a in addrs {
        match tokio::time::timeout(TCP_TIMEOUT, TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Ok(Err(e)) => failures.push(format!("{a}: {e}")),
            Err(_) => failures.push(format!("{a}: no answer within {}s", TCP_TIMEOUT.as_secs())),
        }
    }
    Err(Error::Unreachable(failures.join("; ")))
}

/// One established connection: its outgoing queue and incoming events.
struct Established {
    tx: mpsc::Sender<Outgoing>,
    events: mpsc::Receiver<Event>,
    cert_sha256: [u8; 32],
}

/// Connect, authenticate and start the session tasks. Returns once the server
/// has sent `ServerSync`, so `Client::state()` already holds channels and users.
pub async fn connect(cfg: Config) -> Result<(Client, mpsc::Receiver<Event>), Error> {
    connect_with(cfg, ConnectOptions::default()).await
}

/// Like [`connect`], with tuning and optional automatic reconnecting.
pub async fn connect_with(cfg: Config, opts: ConnectOptions) -> Result<(Client, mpsc::Receiver<Event>), Error> {
    let state = Arc::new(RwLock::new(ServerState::default()));
    let ping_now = Arc::new(Notify::new());
    let est = connect_once(&cfg, &opts, &state, &ping_now).await?;
    let inner = Arc::new(Inner {
        tx: RwLock::new(est.tx),
        state,
        cert_sha256: RwLock::new(est.cert_sha256),
        closed: AtomicBool::new(false),
        ping_now: ping_now.clone(),
    });
    let client = Client { inner: inner.clone() };
    if !opts.auto_reconnect {
        return Ok((client, est.events));
    }
    let (out_tx, out_rx) = mpsc::channel(256);
    tokio::spawn(supervise(Arc::downgrade(&inner), cfg, opts, ping_now, est.events, out_tx));
    Ok((client, out_rx))
}

/// Forwards events and, when the connection ends, reconnects until it works, the user closes the
/// session, or a fatal error occurs. Holds only a weak reference, so dropping every `Client` ends it.
async fn supervise(
    weak: Weak<Inner>,
    cfg: Config,
    opts: ConnectOptions,
    ping_now: Arc<Notify>,
    first_events: mpsc::Receiver<Event>,
    out: mpsc::Sender<Event>,
) {
    let mut events = first_events;
    let mut connected_at = Instant::now();
    let mut quick_drops = 0u32;
    loop {
        // Forward until this connection ends.
        let reason = loop {
            match events.recv().await {
                Some(Event::Disconnected(r)) => break r,
                Some(ev) => {
                    if out.send(ev).await.is_err() {
                        return;
                    }
                }
                None => break None,
            }
        };
        let Some(inner) = weak.upgrade() else { return };
        if inner.closed.load(Ordering::SeqCst) {
            let _ = out.send(Event::Disconnected(reason)).await;
            return;
        }
        // A clean close soon after connecting looks like "the server ended this session because the
        // same name logged in elsewhere". Reconnecting would kick that other device in turn.
        if reason.is_none() && connected_at.elapsed() < opts.quick_drop_window {
            quick_drops += 1;
        } else {
            quick_drops = 0;
        }
        if quick_drops >= opts.max_quick_drops {
            let why = "connection repeatedly closed by the server right after connecting (same user name used on another device?)";
            let _ = out.send(Event::Disconnected(Some(why.to_string()))).await;
            return;
        }
        // Remember where we were, then forget the server's picture: users who left during the outage
        // must not stay on the screen, and session ids change anyway.
        let own_channel = inner.state.read().unwrap().own_channel();
        *inner.state.write().unwrap() = ServerState::default();
        let state = inner.state.clone();
        drop(inner);
        if out.send(Event::Reconnecting { attempt: 0, reason }).await.is_err() {
            return;
        }

        let mut attempt = 0usize;
        let est = loop {
            tokio::time::sleep(opts.wait_before_attempt(attempt)).await;
            attempt += 1;
            match weak.upgrade() {
                Some(i) if !i.closed.load(Ordering::SeqCst) => {}
                _ => return,
            }
            match connect_once(&cfg, &opts, &state, &ping_now).await {
                Ok(e) => break e,
                Err(e) if retryable(&e) => {
                    if out.send(Event::Reconnecting { attempt: attempt as u32, reason: Some(e.to_string()) }).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = out.send(Event::Disconnected(Some(e.to_string()))).await;
                    return;
                }
            }
        };

        // Swap in the new connection. Anything queued for the old one is dropped with the old
        // sender: voice from before the outage must never be replayed.
        let Some(inner) = weak.upgrade() else { return };
        if inner.closed.load(Ordering::SeqCst) {
            return;
        }
        connected_at = Instant::now();
        *inner.cert_sha256.write().unwrap() = est.cert_sha256;
        let new_tx = est.tx;
        *inner.tx.write().unwrap() = new_tx.clone();
        events = est.events;
        let (session, now_in) = {
            let s = inner.state.read().unwrap();
            (s.own_session, s.own_channel())
        };
        drop(inner);
        if let Some(ch) = own_channel.filter(|&c| Some(c) != now_in) {
            let m = proto::UserState { session, channel_id: Some(ch), ..Default::default() };
            let _ = new_tx.send((MessageType::UserState as u16, m.encode_to_vec())).await;
        }
        if out.send(Event::Reconnected).await.is_err() {
            return;
        }
    }
}

async fn connect_once(
    cfg: &Config,
    opts: &ConnectOptions,
    state: &Arc<RwLock<ServerState>>,
    ping_now: &Arc<Notify>,
) -> Result<Established, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier { pin: cfg.pinned_sha256, provider }))
        .with_no_client_auth();
    let tcp = tcp_connect(&cfg.host, cfg.port).await?;
    let name = ServerName::try_from(cfg.host.clone()).map_err(|_| Error::ServerName)?;
    let stream = tokio::time::timeout(TLS_TIMEOUT, TlsConnector::from(Arc::new(tls)).connect(name, tcp))
        .await
        .map_err(|_| Error::Timeout("TLS handshake"))?
        .map_err(|e| if e.to_string().contains("pin mismatch") { Error::PinMismatch } else { Error::Io(e) })?;
    let cert_sha256 = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| sha256(c))
        .unwrap_or([0; 32]);
    let (mut rd, mut wr) = tokio::io::split(stream);

    let version = Version {
        version_v1: Some(VERSION_V1),
        version_v2: Some(VERSION_V2),
        release: Some("MumbleFunkClient".into()),
        os: Some(std::env::consts::OS.into()),
        os_version: None,
    };
    let auth = Authenticate {
        username: Some(cfg.username.clone()),
        password: cfg.password.clone(),
        tokens: cfg.tokens.clone(),
        opus: Some(true),
        ..Default::default()
    };
    write_frame(&mut wr, MessageType::Version as u16, &version.encode_to_vec()).await?;
    write_frame(&mut wr, MessageType::Authenticate as u16, &auth.encode_to_vec()).await?;

    let (ev_tx, ev_rx) = mpsc::channel(256);

    // Handshake: consume messages until ServerSync (or Reject).
    let login = async {
        loop {
            let (ty, payload) = read_frame(&mut rd).await.map_err(|e| match e {
                WireError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => Error::Closed,
                e => e.into(),
            })?;
            if ty == MessageType::Reject as u16 {
                let r = proto::Reject::decode(payload).unwrap_or_default();
                let reason = r.reason.clone().unwrap_or_else(|| "unknown".into());
                return Err(match r.r#type.and_then(|t| proto::reject::RejectType::try_from(t).ok()) {
                    Some(proto::reject::RejectType::UsernameInUse) => Error::UsernameInUse(reason),
                    Some(proto::reject::RejectType::ServerFull) => Error::ServerFull(reason),
                    _ => Error::Rejected(reason),
                });
            }
            if dispatch(state, &ev_tx, ty, payload).await == Some(MessageType::ServerSync) {
                return Ok(state.read().unwrap().own_session.unwrap_or(0));
            }
        }
    };
    let session = tokio::time::timeout(LOGIN_TIMEOUT, login).await.map_err(|_| Error::Timeout("login"))??;
    let _ = ev_tx.send(Event::Connected { session, cert_sha256 }).await;

    let (tx, rx) = mpsc::channel::<Outgoing>(256);
    tokio::spawn(writer(wr, rx, opts.clone(), ping_now.clone()));
    tokio::spawn(reader(rd, state.clone(), ev_tx, opts.clone()));
    Ok(Established { tx, events: ev_rx, cert_sha256 })
}

async fn writer<W: AsyncWrite + Unpin>(mut wr: WriteHalf<W>, mut rx: mpsc::Receiver<Outgoing>, opts: ConnectOptions, ping_now: Arc<Notify>) {
    // An absolute schedule: voice frames arriving every 20 ms must not keep postponing the ping.
    let mut next_ping = Instant::now(); // first ping right away: the latency readout starts early
    loop {
        let ping = tokio::select! {
            m = rx.recv() => match m {
                Some((ty, p)) => {
                    if write_frame(&mut wr, ty, &p).await.is_err() { break }
                    false
                }
                None => break,
            },
            _ = tokio::time::sleep_until(next_ping) => true,
            _ = ping_now.notified() => true,
        };
        if ping {
            let p = proto::Ping { timestamp: Some(now_millis()), ..Default::default() }.encode_to_vec();
            if write_frame(&mut wr, MessageType::Ping as u16, &p).await.is_err() { break }
            next_ping = Instant::now() + opts.ping_interval();
        }
    }
    // Dropping all `Client` handles ends up here: close the TLS session so the reader sees EOF.
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
}

async fn reader<R: AsyncRead + Unpin>(mut rd: ReadHalf<R>, state: Arc<RwLock<ServerState>>, ev: mpsc::Sender<Event>, opts: ConnectOptions) {
    let reason = loop {
        // Every ping is answered, so silence for longer than this means the link is dead (a
        // half-open TCP connection never reports an error by itself).
        match tokio::time::timeout(opts.dead_after(), read_frame(&mut rd)).await {
            Err(_) => break Some("no response from server".to_string()),
            Ok(Ok((ty, payload))) => {
                dispatch(&state, &ev, ty, payload).await;
            }
            Ok(Err(WireError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => break None,
            Ok(Err(e)) => break Some(e.to_string()),
        }
    };
    let _ = ev.send(Event::Disconnected(reason)).await;
}

/// Apply one incoming frame to the state and emit the matching event.
/// Returns the message type if it was a known one.
async fn dispatch(state: &RwLock<ServerState>, ev: &mpsc::Sender<Event>, ty: u16, payload: Bytes) -> Option<MessageType> {
    let mt = MessageType::from_u16(ty)?;
    let event = match mt {
        MessageType::ServerSync => {
            if let Ok(m) = proto::ServerSync::decode(payload) {
                state.write().unwrap().apply_sync(&m);
            }
            None
        }
        MessageType::ChannelState => proto::ChannelState::decode(payload)
            .ok()
            .and_then(|m| state.write().unwrap().apply_channel(&m))
            .map(Event::ChannelUpdated),
        MessageType::ChannelRemove => {
            if let Ok(m) = proto::ChannelRemove::decode(payload) {
                state.write().unwrap().remove_channel(&m);
            }
            None
        }
        MessageType::UserState => proto::UserState::decode(payload)
            .ok()
            .and_then(|m| state.write().unwrap().apply_user(&m))
            .map(Event::UserUpdated),
        MessageType::UserRemove => proto::UserRemove::decode(payload).ok().map(|m| {
            state.write().unwrap().remove_user(&m);
            Event::UserRemoved(m.session)
        }),
        MessageType::TextMessage => proto::TextMessage::decode(payload)
            .ok()
            .map(|m| Event::Text { actor: m.actor, text: m.message }),
        MessageType::PermissionDenied => proto::PermissionDenied::decode(payload)
            .ok()
            .map(|m| Event::PermissionDenied(m.reason.unwrap_or_default())),
        MessageType::Ping => proto::Ping::decode(payload).ok().and_then(|p| p.timestamp).map(|sent| {
            let now = now_millis();
            Event::Latency(now.saturating_sub(sent).min(u32::MAX as u64) as u32)
        }),
        MessageType::UdpTunnel => Some(Event::Voice(payload)),
        MessageType::PluginData => proto::PluginDataTransmission::decode(payload).ok().map(|m| Event::PluginData {
            sender: m.sender_session,
            data_id: m.data_id.unwrap_or_default(),
            data: m.data.map(Bytes::from).unwrap_or_default(),
        }),
        _ => None,
    };
    if let Some(e) = event {
        let _ = ev.send(e).await;
    }
    Some(mt)
}
