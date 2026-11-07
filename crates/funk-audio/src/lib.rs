//! Microphone capture and speaker playback on top of `cpal`, wired to the
//! `funk-core` voice logic. Everything runs at 48 kHz; devices that cannot do
//! 48 kHz are rejected with a clear error (resampling is a later task).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use funk_core::voice::{FRAME_SAMPLES, MixInfo, Mixer, SAMPLE_RATE, Transmitter};

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no {0} device available")]
    NoDevice(&'static str),
    #[error("device does not support 48 kHz {0}")]
    Unsupported(&'static str),
    #[error("audio: {0}")]
    Cpal(String),
    #[error("opus: {0}")]
    Voice(#[from] funk_core::voice::VoiceError),
}

fn cpal_err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Cpal(e.to_string())
}

/// Shared transmit/receive state touched by the audio callbacks and the network side.
pub struct Shared {
    pub tx: Mutex<Transmitter>,
    pub mixer: Mutex<Mixer>,
}

/// Called from the capture callback with ready voice packets. Must not block.
pub type PacketSink = Box<dyn Fn(Vec<u8>) + Send + 'static>;
/// Called from the playback callback whenever the set of speakers changes.
pub type SpeakerSink = Box<dyn Fn(MixInfo) + Send + 'static>;

/// Running audio streams. Dropping it stops capture and playback.
pub struct Audio {
    _input: Stream,
    _output: Stream,
}

fn find_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    what: &'static str,
) -> Result<(StreamConfig, SampleFormat), AudioError> {
    let mut best: Option<cpal::SupportedStreamConfig> = None;
    for c in configs {
        if c.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= c.max_sample_rate() {
            let c = c.with_sample_rate(SAMPLE_RATE);
            // Prefer fewer channels and i16/f32 formats.
            let better = best.as_ref().is_none_or(|b| c.channels() < b.channels());
            if better && matches!(c.sample_format(), SampleFormat::I16 | SampleFormat::F32) {
                best = Some(c);
            }
        }
    }
    let c = best.ok_or(AudioError::Unsupported(what))?;
    Ok((c.config(), c.sample_format()))
}

impl Audio {
    pub fn start(shared: Arc<Shared>, on_packet: PacketSink, on_speakers: SpeakerSink) -> Result<Self, AudioError> {
        let host = cpal::default_host();

        // --- capture ---
        let in_dev = host.default_input_device().ok_or(AudioError::NoDevice("input"))?;
        let (in_cfg, in_fmt) = find_config(in_dev.supported_input_configs().map_err(cpal_err)?, "input")?;
        let in_ch = in_cfg.channels as usize;
        let sh = shared.clone();
        let mut mono: Vec<i16> = Vec::with_capacity(2048);
        let mut handle_in = move |frames: &mut dyn Iterator<Item = i16>| {
            // Cheap early-out while PTT is up: no copying, no locking beyond the flag.
            let Ok(mut tx) = sh.tx.try_lock() else { return };
            if !tx.is_active() {
                return;
            }
            mono.clear();
            mono.extend(frames);
            if let Ok(packets) = tx.push_pcm(&mono) {
                for p in packets {
                    on_packet(p);
                }
            }
        };
        let input = match in_fmt {
            SampleFormat::I16 => in_dev.build_input_stream(
                in_cfg,
                move |d: &[i16], _| handle_in(&mut d.chunks(in_ch).map(|f| f[0])),
                |e| tracing::warn!("input stream error: {e}"),
                None,
            ),
            _ => in_dev.build_input_stream(
                in_cfg,
                move |d: &[f32], _| handle_in(&mut d.chunks(in_ch).map(|f| (f[0].clamp(-1.0, 1.0) * 32767.0) as i16)),
                |e| tracing::warn!("input stream error: {e}"),
                None,
            ),
        }
        .map_err(cpal_err)?;

        // --- playback ---
        let out_dev = host.default_output_device().ok_or(AudioError::NoDevice("output"))?;
        let (out_cfg, out_fmt) = find_config(out_dev.supported_output_configs().map_err(cpal_err)?, "output")?;
        let out_ch = out_cfg.channels as usize;
        let sh = shared.clone();
        let mut queue: VecDeque<i16> = VecDeque::with_capacity(FRAME_SAMPLES * 2);
        let mut last_speaking: Vec<u32> = Vec::new();
        let mut next_sample = move || -> i16 {
            if queue.is_empty() {
                let mut frame = [0i16; FRAME_SAMPLES];
                let info = match sh.mixer.try_lock() {
                    Ok(mut m) => m.mix(&mut frame),
                    Err(_) => MixInfo::default(),
                };
                if info.speaking != last_speaking || !info.ended.is_empty() {
                    last_speaking = info.speaking.clone();
                    on_speakers(info);
                }
                queue.extend(frame);
            }
            queue.pop_front().unwrap_or(0)
        };
        let output = match out_fmt {
            SampleFormat::I16 => out_dev.build_output_stream(
                out_cfg,
                move |d: &mut [i16], _| {
                    for f in d.chunks_mut(out_ch) {
                        f.fill(next_sample());
                    }
                },
                |e| tracing::warn!("output stream error: {e}"),
                None,
            ),
            _ => out_dev.build_output_stream(
                out_cfg,
                move |d: &mut [f32], _| {
                    for f in d.chunks_mut(out_ch) {
                        f.fill(next_sample() as f32 / 32768.0);
                    }
                },
                |e| tracing::warn!("output stream error: {e}"),
                None,
            ),
        }
        .map_err(cpal_err)?;

        input.play().map_err(cpal_err)?;
        output.play().map_err(cpal_err)?;
        Ok(Self { _input: input, _output: output })
    }
}
