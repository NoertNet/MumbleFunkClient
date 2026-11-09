//! Reconnect, dead-link detection and ping scheduling, against a scripted mock server that can
//! drop connections, reject logins and count what it receives.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use funk_core::proto::{ChannelState, Reject, ServerSync, UserState, reject::RejectType};
use funk_core::wire::{MessageType, read_frame, write_frame};
use funk_core::{Client, Config, ConnectOptions, Event, connect_with};
use prost::Message;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

type Conn = TlsStream<TcpStream>;

fn acceptor() -> TlsAcceptor {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![ck.cert.der().clone()], key)
        .unwrap();
    TlsAcceptor::from(Arc::new(cfg))
}

/// Starts a server; `handler(connection_index, stream)` scripts each accepted connection.
/// Returns the port and the number of connections accepted so far.
async fn spawn_server<F, Fut>(handler: F) -> (u16, Arc<AtomicUsize>)
where
    F: Fn(usize, Conn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let acc = acceptor();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { break };
            let idx = c.fetch_add(1, Ordering::SeqCst);
            let (acc, handler) = (acc.clone(), handler.clone());
            tokio::spawn(async move {
                if let Ok(tls) = acc.accept(s).await {
                    handler(idx, tls).await;
                }
            });
        }
    });
    (port, count)
}

/// Reads Version + Authenticate and answers like a server: channels 0..=2, our user, optional extras.
async fn handshake(s: &mut Conn, session: u32, channel: u32, extra: &[(u32, &str)]) {
    let _ = read_frame(s).await.unwrap();
    let _ = read_frame(s).await.unwrap();
    for id in 0u32..=2 {
        let c = ChannelState { channel_id: Some(id), name: Some(format!("K{id}")), ..Default::default() };
        write_frame(s, MessageType::ChannelState as u16, &c.encode_to_vec()).await.unwrap();
    }
    for &(sess, name) in extra {
        let u = UserState { session: Some(sess), name: Some(name.into()), channel_id: Some(0), ..Default::default() };
        write_frame(s, MessageType::UserState as u16, &u.encode_to_vec()).await.unwrap();
    }
    let me = UserState { session: Some(session), name: Some("me".into()), channel_id: Some(channel), ..Default::default() };
    write_frame(s, MessageType::UserState as u16, &me.encode_to_vec()).await.unwrap();
    let sync = ServerSync { session: Some(session), ..Default::default() };
    write_frame(s, MessageType::ServerSync as u16, &sync.encode_to_vec()).await.unwrap();
}

async fn reject(s: &mut Conn, kind: RejectType, reason: &str) {
    let _ = read_frame(s).await;
    let _ = read_frame(s).await;
    let r = Reject { r#type: Some(kind as i32), reason: Some(reason.into()) };
    write_frame(s, MessageType::Reject as u16, &r.encode_to_vec()).await.unwrap();
}

/// Echo pings and hand every other frame to `on_frame` until the client goes away.
async fn serve(s: &mut Conn, mut on_frame: impl FnMut(u16, Vec<u8>)) {
    while let Ok((t, p)) = read_frame(s).await {
        if t == MessageType::Ping as u16 {
            on_frame(t, p.to_vec());
            if write_frame(s, t, &p).await.is_err() {
                break;
            }
        } else {
            on_frame(t, p.to_vec());
        }
    }
}

fn cfg(port: u16) -> Config {
    Config { host: "localhost".into(), port, username: "me".into(), password: None, tokens: vec![], pinned_sha256: None }
}

fn fast(auto: bool) -> ConnectOptions {
    ConnectOptions {
        auto_reconnect: auto,
        ping_foreground: Duration::from_millis(100),
        ping_background: Duration::from_millis(400),
        dead_grace: Duration::from_millis(300),
        backoff: vec![Duration::from_millis(30)],
        ..ConnectOptions::default()
    }
}

async fn wait_for(rx: &mut mpsc::Receiver<Event>, secs: u64, mut pred: impl FnMut(&Event) -> bool) -> Option<Event> {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = end.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ev)) if pred(&ev) => return Some(ev),
            Ok(Some(_)) => {}
            _ => return None,
        }
    }
}

async fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn join_channel_of(t: u16, p: &[u8]) -> Option<u32> {
    (t == MessageType::UserState as u16).then(|| UserState::decode(p).ok()).flatten().and_then(|u| u.channel_id)
}

#[tokio::test]
async fn reconnects_after_drop_clears_stale_users_and_rejoins_the_channel() {
    let rejoined: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let r = rejoined.clone();
    let (port, count) = spawn_server(move |idx, mut s| {
        let r = r.clone();
        async move {
            if idx == 0 {
                handshake(&mut s, 9, 0, &[(8, "gone")]).await;
                // wait for the client's join to channel 2, confirm it, then drop the connection
                loop {
                    let (t, p) = read_frame(&mut s).await.unwrap();
                    if join_channel_of(t, &p) == Some(2) {
                        let moved = UserState { session: Some(9), channel_id: Some(2), ..Default::default() };
                        write_frame(&mut s, MessageType::UserState as u16, &moved.encode_to_vec()).await.unwrap();
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                // dropping `s` closes the connection
            } else {
                handshake(&mut s, 10, 0, &[]).await; // user 8 left meanwhile
                serve(&mut s, |t, p| {
                    if let Some(ch) = join_channel_of(t, &p) {
                        *r.lock().unwrap() = Some(ch);
                    }
                })
                .await;
            }
        }
    })
    .await;

    let (client, mut ev) = connect_with(cfg(port), fast(true)).await.unwrap();
    assert!(client.state().users.contains_key(&8));
    client.join_channel(2).await.unwrap();

    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { attempt: 0, .. })).await.is_some(), "no Reconnecting");
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnected)).await.is_some(), "no Reconnected");

    assert!(!client.state().users.contains_key(&8), "user who left during the outage must be gone");
    assert_eq!(client.state().own_session, Some(10), "new session id");
    assert!(wait_until(3, || *rejoined.lock().unwrap() == Some(2)).await, "previous channel was not re-joined");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn username_in_use_is_retried_until_the_old_session_is_gone() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        match idx {
            0 => {
                handshake(&mut s, 9, 0, &[]).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            1 => reject(&mut s, RejectType::UsernameInUse, "Username already in use").await,
            _ => {
                handshake(&mut s, 11, 0, &[]).await;
                serve(&mut s, |_, _| {}).await;
            }
        }
    })
    .await;
    let (_client, mut ev) = connect_with(cfg(port), fast(true)).await.unwrap();
    let retry = wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { attempt: 1, .. })).await;
    match retry {
        Some(Event::Reconnecting { reason: Some(r), .. }) => assert!(r.contains("already in use"), "{r}"),
        other => panic!("expected a retry notice, got {other:?}"),
    }
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnected)).await.is_some());
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn wrong_password_on_reconnect_is_final() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        if idx == 0 {
            handshake(&mut s, 9, 0, &[]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            reject(&mut s, RejectType::WrongUserPw, "Wrong certificate or password").await;
        }
    })
    .await;
    let (_client, mut ev) = connect_with(cfg(port), fast(true)).await.unwrap();
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { .. })).await.is_some());
    match wait_for(&mut ev, 5, |e| matches!(e, Event::Disconnected(_))).await {
        Some(Event::Disconnected(Some(r))) => assert!(r.contains("password"), "{r}"),
        other => panic!("expected a final Disconnected, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "must not keep trying after a fatal reject");
}

#[tokio::test]
async fn voice_queued_during_the_outage_is_never_replayed() {
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(vec![]));
    let rec = received.clone();
    let (port, _) = spawn_server(move |idx, mut s| {
        let rec = rec.clone();
        async move {
            if idx == 0 {
                handshake(&mut s, 9, 0, &[]).await;
                tokio::time::sleep(Duration::from_millis(80)).await;
            } else {
                handshake(&mut s, 10, 0, &[]).await;
                serve(&mut s, |t, p| {
                    if t == MessageType::UdpTunnel as u16 {
                        rec.lock().unwrap().push(p);
                    }
                })
                .await;
            }
        }
    })
    .await;
    let (client, mut ev) = connect_with(cfg(port), fast(true)).await.unwrap();
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { .. })).await.is_some());
    client.try_send_voice(b"old".to_vec()); // during the outage
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnected)).await.is_some());
    assert!(client.try_send_voice(b"new".to_vec()), "the same handle must reach the new connection");
    assert!(wait_until(3, || !received.lock().unwrap().is_empty()).await);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(*received.lock().unwrap(), vec![b"new".to_vec()]);
}

#[tokio::test]
async fn close_stops_further_attempts() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        handshake(&mut s, 9 + idx as u32, 0, &[]).await;
        if idx == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            serve(&mut s, |_, _| {}).await;
        }
    })
    .await;
    let opts = ConnectOptions { backoff: vec![Duration::from_millis(400)], ..fast(true) };
    let (client, mut ev) = connect_with(cfg(port), opts).await.unwrap();
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { .. })).await.is_some());
    client.close();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "no new connection after close()");
}

#[tokio::test]
async fn dropping_every_handle_stops_the_reconnecting() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        handshake(&mut s, 9 + idx as u32, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    })
    .await;
    let opts = ConnectOptions { backoff: vec![Duration::from_millis(300)], ..fast(true) };
    let (client, mut ev) = connect_with(cfg(port), opts).await.unwrap();
    assert!(wait_for(&mut ev, 5, |e| matches!(e, Event::Reconnecting { .. })).await.is_some());
    drop(client);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn silent_server_is_detected_as_dead_link() {
    // The server completes the handshake and then never answers (half-open connection).
    let (port, _) = spawn_server(|_, mut s| async move {
        handshake(&mut s, 9, 0, &[]).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    })
    .await;
    let (_client, mut ev) = connect_with(cfg(port), fast(false)).await.unwrap();
    match wait_for(&mut ev, 3, |e| matches!(e, Event::Disconnected(_))).await {
        Some(Event::Disconnected(Some(r))) => assert!(r.contains("no response"), "{r}"),
        other => panic!("expected dead-link detection, got {other:?}"),
    }
}

#[tokio::test]
async fn ping_rate_follows_the_background_flag_and_ping_now_is_immediate() {
    let pings: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(vec![]));
    let p = pings.clone();
    let (port, _) = spawn_server(move |_, mut s| {
        let p = p.clone();
        async move {
            handshake(&mut s, 9, 0, &[]).await;
            serve(&mut s, |t, _| {
                if t == MessageType::Ping as u16 {
                    p.lock().unwrap().push(Instant::now());
                }
            })
            .await;
        }
    })
    .await;
    let background = Arc::new(AtomicBool::new(false));
    let flag = background.clone();
    let opts = ConnectOptions {
        is_background: Arc::new(move || flag.load(Ordering::SeqCst)),
        ping_foreground: Duration::from_millis(50),
        ping_background: Duration::from_secs(2),
        dead_grace: Duration::from_secs(5),
        ..ConnectOptions::default()
    };
    let (client, _ev) = connect_with(cfg(port), opts).await.unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;
    let fg = pings.lock().unwrap().len();
    assert!(fg >= 6, "foreground should ping often, got {fg}");

    background.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await; // let the already scheduled ping happen
    let before = pings.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let during = pings.lock().unwrap().len() - before;
    assert!(during <= 1, "background should ping rarely, got {during} in 1 s");

    let n = pings.lock().unwrap().len();
    let t0 = Instant::now();
    client.ping_now();
    assert!(wait_until(1, || pings.lock().unwrap().len() > n).await, "ping_now() did not ping");
    assert!(t0.elapsed() < Duration::from_millis(300), "ping_now() was not immediate");
}

/// A ping is sent even while voice frames stream through the writer every few milliseconds.
#[tokio::test]
async fn ping_is_not_starved_by_voice_traffic() {
    let pings = Arc::new(AtomicUsize::new(0));
    let p = pings.clone();
    let (port, _) = spawn_server(move |_, mut s| {
        let p = p.clone();
        async move {
            handshake(&mut s, 9, 0, &[]).await;
            serve(&mut s, |t, _| {
                if t == MessageType::Ping as u16 {
                    p.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        }
    })
    .await;
    let (client, _ev) = connect_with(cfg(port), ConnectOptions { ping_foreground: Duration::from_millis(100), ..ConnectOptions::default() })
        .await
        .unwrap();
    let c: Client = client.clone();
    let end = Instant::now() + Duration::from_millis(800);
    while Instant::now() < end {
        c.try_send_voice(vec![0, 1, 2]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(pings.load(Ordering::SeqCst) >= 5, "pings starved by voice: {}", pings.load(Ordering::SeqCst));
}

/// The server ends the older session when the same user name logs in again. Two devices with one name
/// must not kick each other forever: after repeated quick clean closes the session gives up.
#[tokio::test]
async fn repeated_quick_clean_closes_stop_the_reconnecting() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        handshake(&mut s, 9 + idx as u32, 0, &[]).await;
        // closed cleanly right away (dropping the stream sends close_notify)
    })
    .await;
    let opts = ConnectOptions { quick_drop_window: Duration::from_secs(5), max_quick_drops: 2, ..fast(true) };
    let (_client, mut ev) = connect_with(cfg(port), opts).await.unwrap();
    match wait_for(&mut ev, 5, |e| matches!(e, Event::Disconnected(_))).await {
        Some(Event::Disconnected(Some(r))) => assert!(r.contains("repeatedly closed"), "{r}"),
        other => panic!("expected the session to give up, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // drop #1: the initial connection; drop #2: the reconnected one -> give up, no third attempt
    assert_eq!(count.load(Ordering::SeqCst), 2, "initial connection + one reconnect, then stop");
}

#[tokio::test]
async fn a_healthy_session_resets_the_quick_drop_counter() {
    let (port, count) = spawn_server(|idx, mut s| async move {
        handshake(&mut s, 9 + idx as u32, 0, &[]).await;
        match idx {
            0 => tokio::time::sleep(Duration::from_millis(400)).await, // long enough: not a quick drop
            1 => {}                                                    // quick drop #1
            2 => tokio::time::sleep(Duration::from_millis(400)).await, // healthy again: resets the counter
            3 => {}                                                    // quick drop #1 again, not #2
            _ => serve(&mut s, |_, _| {}).await,                       // finally stays up
        }
    })
    .await;
    let opts = ConnectOptions { quick_drop_window: Duration::from_millis(250), max_quick_drops: 2, ..fast(true) };
    let (_client, mut ev) = connect_with(cfg(port), opts).await.unwrap();
    // wait for the 4th connection (index 3 dropped quickly, index 4 stays): Reconnected five times in total
    let mut reconnected = 0;
    while reconnected < 4 {
        match wait_for(&mut ev, 8, |e| matches!(e, Event::Reconnected | Event::Disconnected(_))).await {
            Some(Event::Reconnected) => reconnected += 1,
            other => panic!("gave up too early after {reconnected} reconnects: {other:?}"),
        }
    }
    assert!(count.load(Ordering::SeqCst) >= 5);
}
