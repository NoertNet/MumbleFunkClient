//! Microphone capture and speaker playback on top of `cpal`, wired to the
//! `funk-core` voice logic.
//!
//! Streams always use the device's own default format (on Windows shared mode
//! this is the only format that is guaranteed to work). Capture is downmixed to
//! mono and resampled to 48 kHz; playback is resampled from 48 kHz and copied to
//! all device channels.

mod resample;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use funk_core::voice::{FRAME_SAMPLES, MixInfo, Mixer, SAMPLE_RATE, Transmitter};
use resample::Resampler;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no {0} device available")]
    NoDevice(&'static str),
    #[error("{what}: sample format {format:?} is not supported")]
    Unsupported { what: &'static str, format: SampleFormat },
    #[error("{what} ({detail}): {msg}")]
    Cpal { what: &'static str, detail: String, msg: String },
    #[error("opus: {0}")]
    Voice(#[from] funk_core::voice::VoiceError),
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

fn err(what: &'static str, cfg: &StreamConfig, e: impl std::fmt::Display) -> AudioError {
    AudioError::Cpal {
        what,
        detail: format!("{} Hz, {} ch", cfg.sample_rate, cfg.channels),
        msg: e.to_string(),
    }
}

fn build_input<T>(dev: &Device, cfg: &StreamConfig, mut f: impl FnMut(&[f32]) + Send + 'static) -> Result<Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels as usize;
    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    dev.build_input_stream(
        cfg.clone(),
        move |data: &[T], _| {
            mono.clear();
            mono.extend(data.chunks(ch).map(|fr| fr.iter().map(|&s| <f32 as FromSample<T>>::from_sample_(s)).sum::<f32>() / ch as f32));
            f(&mono);
        },
        |e| tracing::warn!("input stream error: {e}"),
        None,
    )
    .map_err(|e| err("audio input", cfg, e))
}

fn build_output<T>(dev: &Device, cfg: &StreamConfig, mut next: impl FnMut() -> f32 + Send + 'static) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    dev.build_output_stream(
        cfg.clone(),
        move |data: &mut [T], _| {
            for fr in data.chunks_mut(ch) {
                fr.fill(<T as FromSample<f32>>::from_sample_(next()));
            }
        },
        |e| tracing::warn!("output stream error: {e}"),
        None,
    )
    .map_err(|e| err("audio output", cfg, e))
}

impl Audio {
    pub fn start(shared: Arc<Shared>, on_packet: PacketSink, on_speakers: SpeakerSink) -> Result<Self, AudioError> {
        let host = cpal::default_host();

        // --- capture: device format -> mono -> 48 kHz -> i16 -> Opus ---
        let in_dev = host.default_input_device().ok_or(AudioError::NoDevice("input"))?;
        let in_conf = in_dev.default_input_config().map_err(|e| AudioError::Cpal {
            what: "audio input",
            detail: "default config".into(),
            msg: e.to_string(),
        })?;
        let in_cfg: StreamConfig = in_conf.config();
        let in_fmt = in_conf.sample_format();
        let sh = shared.clone();
        let mut rs = Resampler::new(in_cfg.sample_rate, SAMPLE_RATE);
        let (mut r48, mut pcm) = (Vec::<f32>::new(), Vec::<i16>::new());
        let on_mono = move |mono: &[f32]| {
            // Cheap early-out while PTT is up: no resampling, no encoding.
            let Ok(mut tx) = sh.tx.try_lock() else { return };
            if !tx.is_active() {
                return;
            }
            r48.clear();
            rs.process(mono, &mut r48);
            pcm.clear();
            pcm.extend(r48.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            if let Ok(packets) = tx.push_pcm(&pcm) {
                for p in packets {
                    on_packet(p);
                }
            }
        };
        let input = match in_fmt {
            SampleFormat::I16 => build_input::<i16>(&in_dev, &in_cfg, on_mono),
            SampleFormat::I32 => build_input::<i32>(&in_dev, &in_cfg, on_mono),
            SampleFormat::U16 => build_input::<u16>(&in_dev, &in_cfg, on_mono),
            SampleFormat::F32 => build_input::<f32>(&in_dev, &in_cfg, on_mono),
            format => return Err(AudioError::Unsupported { what: "audio input", format }),
        }?;

        // --- playback: mixer (48 kHz mono) -> device rate -> all channels ---
        let out_dev = host.default_output_device().ok_or(AudioError::NoDevice("output"))?;
        let out_conf = out_dev.default_output_config().map_err(|e| AudioError::Cpal {
            what: "audio output",
            detail: "default config".into(),
            msg: e.to_string(),
        })?;
        let out_cfg: StreamConfig = out_conf.config();
        let out_fmt = out_conf.sample_format();
        let sh = shared.clone();
        let mut rs = Resampler::new(SAMPLE_RATE, out_cfg.sample_rate);
        let mut ready: VecDeque<f32> = VecDeque::with_capacity(FRAME_SAMPLES * 2);
        let mut last_speaking: Vec<u32> = Vec::new();
        let (mut frame, mut conv) = ([0i16; FRAME_SAMPLES], Vec::<f32>::new());
        let mut src = Vec::<f32>::with_capacity(FRAME_SAMPLES);
        let next = move || -> f32 {
            while ready.is_empty() {
                let info = match sh.mixer.try_lock() {
                    Ok(mut m) => m.mix(&mut frame),
                    Err(_) => {
                        frame.fill(0);
                        MixInfo::default()
                    }
                };
                if info.speaking != last_speaking || !info.ended.is_empty() {
                    last_speaking = info.speaking.clone();
                    on_speakers(info);
                }
                src.clear();
                src.extend(frame.iter().map(|&s| s as f32 / 32768.0));
                conv.clear();
                rs.process(&src, &mut conv);
                ready.extend(conv.iter().copied());
            }
            ready.pop_front().unwrap_or(0.0)
        };
        let output = match out_fmt {
            SampleFormat::I16 => build_output::<i16>(&out_dev, &out_cfg, next),
            SampleFormat::I32 => build_output::<i32>(&out_dev, &out_cfg, next),
            SampleFormat::U16 => build_output::<u16>(&out_dev, &out_cfg, next),
            SampleFormat::F32 => build_output::<f32>(&out_dev, &out_cfg, next),
            format => return Err(AudioError::Unsupported { what: "audio output", format }),
        }?;

        input.play().map_err(|e| err("audio input", &in_cfg, e))?;
        output.play().map_err(|e| err("audio output", &out_cfg, e))?;
        tracing::info!(
            "audio: input {} Hz {} ch {:?}, output {} Hz {} ch {:?}",
            in_cfg.sample_rate, in_cfg.channels, in_fmt, out_cfg.sample_rate, out_cfg.channels, out_fmt
        );
        Ok(Self { _input: input, _output: output })
    }
}
