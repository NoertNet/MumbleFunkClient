//! Voice path: packet format, PTT transmitter (PCM → Opus) and the receive mixer
//! with per-sender jitter buffer (Opus → PCM). Pure logic, no audio device access.
//!
//! Mumble 1.5 protobuf audio is used. Over the TCP tunnel a packet is
//! `0x00 | MumbleUDP.Audio` (audio) or `0x01 | ...` (ping, ignored here).

use std::collections::{BTreeMap, HashMap, VecDeque};

use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bitrate, Channels, SampleRate};
use prost::Message;

use crate::proto::udp::Audio;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms Opus frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;
/// Mumble counts audio in 10 ms units.
const UNITS_PER_FRAME: u64 = 2;
/// Packets buffered before playback starts (3 × 20 ms).
const PREBUFFER: usize = 3;
/// Packets held per sender before we skip ahead to bound latency.
const MAX_PENDING: usize = 25;
/// Empty mixer ticks (20 ms each) without terminator before a stream is dropped.
const STALL_TICKS: u32 = 25;

const TAG_AUDIO: u8 = 0;

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("opus: {0}")]
    Opus(#[from] audiopus::Error),
}

/// A decoded incoming voice packet (still Opus-compressed).
#[derive(Debug, Clone, PartialEq)]
pub struct VoicePacket {
    pub sender: u32,
    pub frame_number: u64,
    pub opus: Vec<u8>,
    pub terminator: bool,
    /// 0 = normal speech, others: shout/whisper/listener.
    pub context: u32,
}

pub fn encode_packet(frame_number: u64, opus: &[u8], terminator: bool) -> Vec<u8> {
    let a = Audio {
        header: Some(crate::proto::udp::audio::Header::Target(0)),
        frame_number,
        opus_data: opus.to_vec(),
        is_terminator: terminator,
        ..Default::default()
    };
    let mut out = Vec::with_capacity(a.encoded_len() + 1);
    out.push(TAG_AUDIO);
    a.encode(&mut out).expect("vec grows");
    out
}

/// Parse a tunnelled packet; `None` for pings, legacy format or garbage.
pub fn decode_packet(data: &[u8]) -> Option<VoicePacket> {
    let (&tag, body) = data.split_first()?;
    if tag != TAG_AUDIO {
        return None;
    }
    let a = Audio::decode(body).ok()?;
    let context = match a.header {
        Some(crate::proto::udp::audio::Header::Context(c)) => c,
        _ => 0,
    };
    Some(VoicePacket {
        sender: a.sender_session,
        frame_number: a.frame_number,
        opus: a.opus_data,
        terminator: a.is_terminator,
        context,
    })
}

/// Turns PCM captured while PTT is held into voice packets.
pub struct Transmitter {
    enc: Encoder,
    buf: Vec<i16>,
    frame_number: u64,
    active: bool,
    out: [u8; 1275],
}

impl Transmitter {
    pub fn new() -> Result<Self, VoiceError> {
        let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
        enc.set_bitrate(Bitrate::BitsPerSecond(24_000))?;
        enc.set_vbr(true)?;
        Ok(Self { enc, buf: Vec::with_capacity(FRAME_SAMPLES * 2), frame_number: 0, active: false, out: [0; 1275] })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// PTT pressed: begin a new stream.
    pub fn start(&mut self) {
        self.buf.clear();
        self.active = true;
        // A new stream starts at a fresh position; receivers key streams by sender.
        self.frame_number = 0;
    }

    /// Feed captured samples (any length). Returns ready packets. Ignored while PTT is up.
    pub fn push_pcm(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, VoiceError> {
        if !self.active {
            return Ok(vec![]);
        }
        self.buf.extend_from_slice(pcm);
        let mut packets = Vec::new();
        while self.buf.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = self.buf.drain(..FRAME_SAMPLES).collect();
            packets.push(self.encode_frame(&frame, false)?);
        }
        Ok(packets)
    }

    /// PTT released: flush the remainder (zero padded) as the terminating packet.
    pub fn stop(&mut self) -> Result<Option<Vec<u8>>, VoiceError> {
        if !self.active {
            return Ok(None);
        }
        self.active = false;
        let mut frame = std::mem::take(&mut self.buf);
        frame.resize(FRAME_SAMPLES, 0);
        Ok(Some(self.encode_frame(&frame, true)?))
    }

    fn encode_frame(&mut self, frame: &[i16], terminator: bool) -> Result<Vec<u8>, VoiceError> {
        let n = self.enc.encode(frame, &mut self.out)?;
        let p = encode_packet(self.frame_number, &self.out[..n], terminator);
        self.frame_number += UNITS_PER_FRAME;
        Ok(p)
    }
}

struct Stream {
    dec: Decoder,
    pending: BTreeMap<u64, (Vec<u8>, bool)>,
    pcm: VecDeque<i16>,
    next: Option<u64>,
    primed: bool,
    got_terminator: bool,
    stalled: u32,
}

impl Stream {
    fn new() -> Result<Self, VoiceError> {
        Ok(Self {
            dec: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            pending: BTreeMap::new(),
            pcm: VecDeque::new(),
            next: None,
            primed: false,
            got_terminator: false,
            stalled: 0,
        })
    }

    fn push(&mut self, p: VoicePacket) {
        let next = *self.next.get_or_insert(p.frame_number);
        if p.frame_number < next {
            return; // too late
        }
        self.pending.insert(p.frame_number, (p.opus, p.terminator));
        self.got_terminator |= p.terminator;
        if self.pending.len() > MAX_PENDING {
            // Far behind: jump to the oldest packet we still have.
            self.next = self.pending.keys().next().copied();
        }
    }

    /// Decode as far as needed to have `need` samples queued. Returns true if any data was produced.
    fn fill(&mut self, need: usize) -> bool {
        if !self.primed {
            if self.pending.len() < PREBUFFER && !self.got_terminator {
                return false;
            }
            self.primed = true;
        }
        let mut buf = [0i16; FRAME_SAMPLES * 6];
        let mut produced = false;
        while self.pcm.len() < need {
            let Some(next) = self.next else { break };
            let (n, units) = if let Some((data, _)) = self.pending.remove(&next) {
                let Ok(n) = self.dec.decode(Some(&data[..]), &mut buf[..], false) else {
                    self.next = Some(next + UNITS_PER_FRAME);
                    continue;
                };
                (n, (n / (SAMPLE_RATE as usize / 100)) as u64)
            } else if self.pending.range(next..).next().is_some() {
                // Gap: conceal one frame.
                let Ok(n) = self.dec.decode(None::<&[u8]>, &mut buf[..FRAME_SAMPLES], false) else { break };
                (n, UNITS_PER_FRAME)
            } else {
                break;
            };
            self.next = Some(next + units.max(1));
            self.pcm.extend(&buf[..n]);
            produced = true;
        }
        produced
    }

    fn finished(&self) -> bool {
        self.pcm.is_empty() && self.pending.is_empty() && (self.got_terminator || self.stalled >= STALL_TICKS)
    }
}

/// Result of one [`Mixer::mix`] tick.
#[derive(Debug, Default, PartialEq)]
pub struct MixInfo {
    /// Senders that contributed audio to this frame.
    pub speaking: Vec<u32>,
    /// Senders whose stream finished during this tick.
    pub ended: Vec<u32>,
}

/// Mixes all incoming senders into one mono stream, one 20 ms frame per call.
#[derive(Default)]
pub struct Mixer {
    streams: HashMap<u32, Stream>,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, p: VoicePacket) -> Result<(), VoiceError> {
        if !self.streams.contains_key(&p.sender) {
            self.streams.insert(p.sender, Stream::new()?);
        }
        self.streams.get_mut(&p.sender).unwrap().push(p);
        Ok(())
    }

    /// Produce the next 20 ms frame (call every 20 ms from the audio clock).
    pub fn mix(&mut self, out: &mut [i16; FRAME_SAMPLES]) -> MixInfo {
        let mut acc = [0i32; FRAME_SAMPLES];
        let mut info = MixInfo::default();
        for (&session, s) in self.streams.iter_mut() {
            let had = s.fill(FRAME_SAMPLES);
            if s.pcm.len() >= FRAME_SAMPLES {
                s.stalled = 0;
                for a in acc.iter_mut() {
                    *a += s.pcm.pop_front().unwrap() as i32;
                }
                info.speaking.push(session);
            } else if s.pcm.is_empty() && !had {
                if s.primed {
                    s.stalled += 1;
                }
            } else {
                // Partial tail (end of stream): play what is left, padded with silence.
                for a in acc.iter_mut() {
                    *a += s.pcm.pop_front().unwrap_or(0) as i32;
                }
                info.speaking.push(session);
            }
            if s.finished() {
                info.ended.push(session);
            }
        }
        for s in &info.ended {
            self.streams.remove(s);
        }
        for (o, a) in out.iter_mut().zip(acc) {
            *o = a.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        info.speaking.sort_unstable();
        info.ended.sort_unstable();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, phase: usize) -> Vec<i16> {
        (0..n)
            .map(|i| ((((i + phase) as f32) * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i16)
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn packets(frames: usize) -> Vec<VoicePacket> {
        let mut tx = Transmitter::new().unwrap();
        tx.start();
        let mut out = tx.push_pcm(&tone(FRAME_SAMPLES * frames, 0)).unwrap();
        out.push(tx.stop().unwrap().unwrap());
        out.iter()
            .map(|p| {
                let mut v = decode_packet(p).unwrap();
                v.sender = 5;
                v
            })
            .collect()
    }

    #[test]
    fn packet_roundtrip() {
        let p = encode_packet(42, b"xyz", true);
        let d = decode_packet(&p).unwrap();
        assert_eq!((d.frame_number, d.opus.as_slice(), d.terminator), (42, &b"xyz"[..], true));
        assert!(decode_packet(&[1, 2, 3]).is_none());
    }

    #[test]
    fn transmitter_marks_only_last_packet_and_counts_10ms_units() {
        let pk = packets(5);
        assert_eq!(pk.len(), 6);
        assert!(pk[..5].iter().all(|p| !p.terminator));
        assert!(pk[5].terminator);
        assert_eq!(pk.iter().map(|p| p.frame_number).collect::<Vec<_>>(), [0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn idle_transmitter_sends_nothing() {
        let mut tx = Transmitter::new().unwrap();
        assert!(tx.push_pcm(&tone(FRAME_SAMPLES, 0)).unwrap().is_empty());
        assert!(tx.stop().unwrap().is_none());
    }

    fn run(mixer: &mut Mixer, ticks: usize) -> (Vec<f64>, Vec<MixInfo>) {
        let mut levels = vec![];
        let mut infos = vec![];
        let mut out = [0i16; FRAME_SAMPLES];
        for _ in 0..ticks {
            let i = mixer.mix(&mut out);
            levels.push(rms(&out));
            infos.push(i);
        }
        (levels, infos)
    }

    #[test]
    fn mixer_plays_stream_and_reports_end() {
        let mut m = Mixer::new();
        for p in packets(10) {
            m.push(p).unwrap();
        }
        let (levels, infos) = run(&mut m, 14);
        assert!(levels[..10].iter().all(|&l| l > 1000.0), "{levels:?}");
        assert!(infos[0].speaking == [5]);
        assert_eq!(infos.iter().filter(|i| i.ended == [5]).count(), 1);
        assert!(infos.last().unwrap().speaking.is_empty());
    }

    #[test]
    fn mixer_reorders_and_conceals_loss() {
        let mut pk = packets(10);
        pk.swap(2, 3); // reorder
        pk.remove(6); // loss
        let mut m = Mixer::new();
        for p in pk {
            m.push(p).unwrap();
        }
        let (levels, infos) = run(&mut m, 12);
        assert!(levels[..10].iter().all(|&l| l > 500.0), "{levels:?}");
        assert!(infos.iter().any(|i| i.ended == [5]));
    }

    #[test]
    fn single_packet_blip_is_played() {
        let mut tx = Transmitter::new().unwrap();
        tx.start();
        let _ = tx.push_pcm(&tone(100, 0)).unwrap();
        let mut p = decode_packet(&tx.stop().unwrap().unwrap()).unwrap();
        p.sender = 1;
        let mut m = Mixer::new();
        m.push(p).unwrap();
        let (levels, infos) = run(&mut m, 2);
        assert!(levels[0] > 100.0);
        assert!(infos.iter().any(|i| i.ended == [1]));
    }

    #[test]
    fn stalled_stream_without_terminator_times_out() {
        let mut pk = packets(5);
        pk.pop(); // drop terminator
        let mut m = Mixer::new();
        for p in pk {
            m.push(p).unwrap();
        }
        let (_, infos) = run(&mut m, 5 + STALL_TICKS as usize + 2);
        assert!(infos.iter().any(|i| i.ended == [5]));
    }
}
