use std::sync::Arc;

use funk_core::proto::{ChannelState, Reject, ServerSync, UserState, Version};
use funk_core::wire::{MessageType, read_frame, write_frame};
use funk_core::{Config, Error, Event, connect};
use prost::Message;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

fn acceptor() -> (TlsAcceptor, [u8; 32]) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = ck.cert.der().clone();
    let fp: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, &der).as_ref().try_into().unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der], key)
        .unwrap();
    (TlsAcceptor::from(Arc::new(cfg)), fp)
}

fn cfg(port: u16, pin: Option<[u8; 32]>) -> Config {
    Config { host: "localhost".into(), port, username: "tester".into(), password: None, tokens: vec![], pinned_sha256: pin }
}

#[tokio::test]
async fn handshake_state_and_join() {
    let (acc, fp) = acceptor();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let mut s = acc.accept(s).await.unwrap();
        let (t, p) = read_frame(&mut s).await.unwrap();
        assert_eq!(t, MessageType::Version as u16);
        assert!(Version::decode(p).unwrap().version_v2.is_some());
        let (t, _) = read_frame(&mut s).await.unwrap();
        assert_eq!(t, MessageType::Authenticate as u16);
        for (id, name) in [(0u32, "Root"), (1, "Kanal 1")] {
            let c = ChannelState { channel_id: Some(id), name: Some(name.into()), parent: (id > 0).then_some(0), ..Default::default() };
            write_frame(&mut s, MessageType::ChannelState as u16, &c.encode_to_vec()).await.unwrap();
        }
        let u = UserState { session: Some(9), name: Some("tester".into()), channel_id: Some(0), ..Default::default() };
        write_frame(&mut s, MessageType::UserState as u16, &u.encode_to_vec()).await.unwrap();
        let sync = ServerSync { session: Some(9), welcome_text: Some("hi".into()), ..Default::default() };
        write_frame(&mut s, MessageType::ServerSync as u16, &sync.encode_to_vec()).await.unwrap();
        // Expect the client's channel join (pings may arrive first; ignore them).
        let (t, p) = loop {
            let f = read_frame(&mut s).await.unwrap();
            if f.0 != MessageType::Ping as u16 {
                break f;
            }
        };
        assert_eq!(t, MessageType::UserState as u16);
        let m = UserState::decode(p).unwrap();
        assert_eq!((m.session, m.channel_id), (Some(9), Some(1)));
        let moved = UserState { session: Some(9), channel_id: Some(1), ..Default::default() };
        write_frame(&mut s, MessageType::UserState as u16, &moved.encode_to_vec()).await.unwrap();
    });

    let (client, mut ev) = connect(cfg(port, Some(fp))).await.unwrap();
    let st = client.state();
    assert_eq!(st.own_session, Some(9));
    assert_eq!(st.channels.len(), 2);
    assert_eq!(st.own_channel(), Some(0));
    assert_eq!(st.welcome_text, "hi");

    client.join_channel(1).await.unwrap();
    loop {
        match ev.recv().await.unwrap() {
            Event::UserUpdated(9) if client.state().own_channel() == Some(1) => break,
            _ => {}
        }
    }
    server.await.unwrap();
}

#[tokio::test]
async fn wrong_pin_is_refused() {
    let (acc, _fp) = acceptor();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let _ = acc.accept(s).await;
    });
    match connect(cfg(port, Some([0; 32]))).await {
        Err(Error::PinMismatch) => {}
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("must not connect"),
    }
}

#[tokio::test]
async fn reject_is_reported() {
    let (acc, _) = acceptor();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let mut s = acc.accept(s).await.unwrap();
        let _ = read_frame(&mut s).await;
        let _ = read_frame(&mut s).await;
        let r = Reject { reason: Some("wrong password".into()), ..Default::default() };
        write_frame(&mut s, MessageType::Reject as u16, &r.encode_to_vec()).await.unwrap();
    });
    match connect(cfg(port, None)).await {
        Err(Error::Rejected(r)) => assert_eq!(r, "wrong password"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("must not connect"),
    }
}

#[tokio::test]
async fn closed_port_reports_unreachable_with_address() {
    // Bind then drop to get a port that is guaranteed closed.
    let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    match connect(cfg(port, None)).await {
        Err(Error::Unreachable(d)) => assert!(d.contains(&port.to_string()), "{d}"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("must not connect"),
    }
}

#[tokio::test]
async fn ping_reply_becomes_latency_event() {
    let (acc, _) = acceptor();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let mut s = acc.accept(s).await.unwrap();
        let _ = read_frame(&mut s).await;
        let _ = read_frame(&mut s).await;
        let sync = ServerSync { session: Some(1), ..Default::default() };
        write_frame(&mut s, MessageType::ServerSync as u16, &sync.encode_to_vec()).await.unwrap();
        // Echo every ping back unchanged, like a real server does.
        loop {
            let Ok((t, p)) = read_frame(&mut s).await else { break };
            if t == MessageType::Ping as u16 {
                write_frame(&mut s, t, &p).await.unwrap();
            }
        }
    });
    let (_client, mut ev) = connect(cfg(port, None)).await.unwrap();
    let ms = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Event::Latency(ms) = ev.recv().await.unwrap() {
                return ms;
            }
        }
    })
    .await
    .expect("no latency event");
    assert!(ms < 1000, "implausible latency {ms}");
}
