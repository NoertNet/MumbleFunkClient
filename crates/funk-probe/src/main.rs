//! Command line probe for a Mumble-Funk server. Needs no audio hardware:
//! it connects, prints channels/users/events and can transmit a test tone via the PTT path.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use funk_core::voice::{FRAME_SAMPLES, Mixer, Transmitter, decode_packet};
use funk_core::{Config, Event, connect};

const USAGE: &str = "usage: funk-probe <host> <user> [--port N] [--password P] [--pin HEX64] \
[--channel ID] [--tone SECS] [--listen SECS]\n\
  --tone SECS    after joining, transmit a 440 Hz test tone for SECS seconds (PTT down/up)\n\
  --listen SECS  stay connected for SECS seconds and print voice/plugin events (default 5)";

fn parse_pin(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        bail!("--pin needs 64 hex characters");
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("--pin is not hex")?;
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(host), Some(user)) = (args.next(), args.next()) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let (mut port, mut password, mut pin, mut channel, mut tone, mut listen) = (64738u16, None, None, None, None::<u64>, 5u64);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--port" => port = val()?.parse()?,
            "--password" => password = Some(val()?),
            "--pin" => pin = Some(parse_pin(&val()?)?),
            "--channel" => channel = Some(val()?.parse::<u32>()?),
            "--tone" => tone = Some(val()?.parse()?),
            "--listen" => listen = val()?.parse()?,
            _ => bail!("unknown argument {a}\n{USAGE}"),
        }
    }

    let cfg = Config { host, port, username: user, password, tokens: vec![], pinned_sha256: pin };
    let (client, mut events) = connect(cfg).await.context("connect")?;
    let st = client.state();
    println!("connected as session {:?}; welcome: {}", st.own_session, st.welcome_text.trim());
    let mut chans: Vec<_> = st.channels.values().collect();
    chans.sort_by_key(|c| c.id);
    for c in chans {
        let users: Vec<_> = st.users_in(c.id).iter().map(|u| format!("{}#{}", u.name, u.session)).collect();
        println!("  channel {} \"{}\" {}", c.id, c.name, users.join(", "));
    }

    if let Some(ch) = channel {
        client.join_channel(ch).await?;
        println!("joining channel {ch}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    if let Some(secs) = tone {
        let c = client.clone();
        tokio::spawn(async move {
            let mut tx = Transmitter::new().expect("opus");
            println!("PTT down, sending {secs}s tone");
            tx.start();
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let mut n = 0usize;
            for _ in 0..secs * 50 {
                tick.tick().await;
                let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                    .map(|i| (((n + i) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 6000.0) as i16)
                    .collect();
                n += FRAME_SAMPLES;
                for p in tx.push_pcm(&pcm).expect("encode") {
                    if c.send_voice(p).await.is_err() {
                        return;
                    }
                }
            }
            if let Some(p) = tx.stop().expect("encode") {
                let _ = c.send_voice(p).await;
            }
            println!("PTT up (terminator sent)");
        });
    }

    let deadline = tokio::time::sleep(Duration::from_secs(listen.max(tone.map_or(0, |t| t + 2))));
    tokio::pin!(deadline);
    let mut mixer = Mixer::new();
    let mut last_speaker: Option<u32> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut out = [0i16; FRAME_SAMPLES];
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = tick.tick() => {
                let info = mixer.mix(&mut out);
                if info.speaking.first().copied() != last_speaker {
                    last_speaker = info.speaking.first().copied();
                    println!("speaking: {:?}", info.speaking);
                }
                for s in info.ended { println!("stream from session {s} ended"); }
            }
            ev = events.recv() => match ev {
                Some(Event::Voice(b)) => match decode_packet(&b) {
                    Some(p) => { let _ = mixer.push(p); }
                    None => println!("voice: unparsed {} bytes (first byte {:#x})", b.len(), b.first().copied().unwrap_or(0)),
                },
                Some(Event::PluginData { sender, data_id, data }) => {
                    println!("plugin data id={data_id} from={sender:?}: {}", String::from_utf8_lossy(&data));
                }
                Some(Event::Connected { cert_sha256, .. }) => println!("cert sha256: {}", hex(&cert_sha256)),
                Some(Event::Text { actor, text }) => println!("text from {actor:?}: {text}"),
                Some(Event::PermissionDenied(r)) => println!("permission denied: {r}"),
                Some(Event::Disconnected(r)) => { println!("disconnected: {r:?}"); break }
                Some(e) => println!("{e:?}"),
                None => break,
            }
        }
    }
    Ok(())
}
