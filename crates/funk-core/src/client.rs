//! TLS connection, handshake and the read/write tasks of one server session.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use prost::Message;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::proto::{self, Authenticate, Version};
use crate::state::ServerState;
use crate::wire::{MessageType, WireError, read_frame, write_frame};

const PING_INTERVAL: Duration = Duration::from_secs(15);
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
    #[error("connection closed")]
    Closed,
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
    Disconnected(Option<String>),
}

type Outgoing = (u16, Vec<u8>);

/// Cheap, cloneable handle to a running session.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::Sender<Outgoing>,
    state: Arc<RwLock<ServerState>>,
}

impl Client {
    pub fn state(&self) -> ServerState {
        self.state.read().unwrap().clone()
    }

    async fn send<M: Message>(&self, ty: MessageType, m: &M) -> Result<(), Error> {
        self.tx.send((ty as u16, m.encode_to_vec())).await.map_err(|_| Error::Closed)
    }

    pub async fn join_channel(&self, channel_id: u32) -> Result<(), Error> {
        let session = self.state.read().unwrap().own_session;
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
        self.tx.try_send((MessageType::UdpTunnel as u16, packet)).is_ok()
    }

    /// Send an already framed voice packet through the TCP tunnel.
    pub async fn send_voice(&self, packet: Vec<u8>) -> Result<(), Error> {
        self.tx.send((MessageType::UdpTunnel as u16, packet)).await.map_err(|_| Error::Closed)
    }
}

#[derive(Debug)]
struct Verifier {
    pin: Option<[u8; 32]>,
    provider: Arc<rustls::crypto::CryptoProvider>,
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

/// Connect, authenticate and start the session tasks. Returns once the server
/// has sent `ServerSync`, so `Client::state()` already holds channels and users.
pub async fn connect(cfg: Config) -> Result<(Client, mpsc::Receiver<Event>), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier { pin: cfg.pinned_sha256, provider }))
        .with_no_client_auth();
    let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).await?;
    tcp.set_nodelay(true)?;
    let name = ServerName::try_from(cfg.host.clone()).map_err(|_| Error::ServerName)?;
    let stream = TlsConnector::from(Arc::new(tls)).connect(name, tcp).await.map_err(|e| {
        if e.to_string().contains("pin mismatch") { Error::PinMismatch } else { Error::Io(e) }
    })?;
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

    let state = Arc::new(RwLock::new(ServerState::default()));
    let (ev_tx, ev_rx) = mpsc::channel(256);

    // Handshake: consume messages until ServerSync (or Reject).
    let session = loop {
        let (ty, payload) = read_frame(&mut rd).await.map_err(|e| match e {
            WireError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => Error::Closed,
            e => e.into(),
        })?;
        if ty == MessageType::Reject as u16 {
            let r = proto::Reject::decode(payload).unwrap_or_default();
            return Err(Error::Rejected(r.reason.unwrap_or_else(|| "unknown".into())));
        }
        if dispatch(&state, &ev_tx, ty, payload).await == Some(MessageType::ServerSync) {
            break state.read().unwrap().own_session.unwrap_or(0);
        }
    };
    let _ = ev_tx.send(Event::Connected { session, cert_sha256 }).await;

    let (tx, rx) = mpsc::channel::<Outgoing>(256);
    tokio::spawn(writer(wr, rx));
    tokio::spawn(reader(rd, state.clone(), ev_tx));
    Ok((Client { tx, state }, ev_rx))
}

async fn writer<W: AsyncWrite + Unpin>(mut wr: WriteHalf<W>, mut rx: mpsc::Receiver<Outgoing>) {
    let mut tick = tokio::time::interval(PING_INTERVAL);
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some((ty, p)) => if write_frame(&mut wr, ty, &p).await.is_err() { break },
                None => break,
            },
            _ = tick.tick() => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                let p = proto::Ping { timestamp: Some(ts), ..Default::default() }.encode_to_vec();
                if write_frame(&mut wr, MessageType::Ping as u16, &p).await.is_err() { break }
            }
        }
    }
    // Dropping all `Client` handles ends up here: close the TLS session so the reader sees EOF.
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
}

async fn reader<R: AsyncRead + Unpin>(mut rd: ReadHalf<R>, state: Arc<RwLock<ServerState>>, ev: mpsc::Sender<Event>) {
    let reason = loop {
        match read_frame(&mut rd).await {
            Ok((ty, payload)) => {
                dispatch(&state, &ev, ty, payload).await;
            }
            Err(WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break None,
            Err(e) => break Some(e.to_string()),
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
