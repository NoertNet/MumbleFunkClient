//! Microphone capture and speaker playback on top of `cpal`, wired to the
//! `funk-core` voice logic.
//!
//! Streams always use the device's own default format (on Windows shared mode
//! this is the only format that is guaranteed to work). Capture is downmixed to
//! mono and resampled to 48 kHz; playback is resampled from 48 kHz and copied to
//! all device channels.

mod meter;
mod resample;

pub use meter::BANDS;
use meter::Meter;

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
    #[error("could not open any {what} device. Tried: {tried}")]
    AllFailed { what: &'static str, tried: String },
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
pub type PacketSink = Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>;
/// Called from the playback callback whenever the set of speakers changes.
pub type SpeakerSink = Arc<dyn Fn(MixInfo) + Send + Sync + 'static>;

/// Spectrum of what is currently heard (`tx == false`) or spoken (`tx == true`), ~25 times a second
/// while there is sound. Called from the audio threads, so it must not block.
pub struct Level {
    pub tx: bool,
    pub bands: [u8; BANDS],
}
pub type LevelSink = Arc<dyn Fn(Level) + Send + Sync + 'static>;

/// Minimum time between two level reports.
const LEVEL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);

/// Devices to use, by the name `list_devices` reports. `None` = system default.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct DevicePrefs {
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone, Default, Debug)]
pub struct DeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

/// Names of all input and output devices (for a settings dialog).
pub fn list_devices() -> DeviceList {
    let host = cpal::default_host();
    let names = |kind| {
        let mut v: Vec<String> = all_devices(&host, kind).iter().map(name_of).collect();
        v.sort();
        v.dedup();
        v
    };
    DeviceList {
        inputs: names(Kind::Input),
        outputs: names(Kind::Output),
        default_input: default_device(&host, Kind::Input).as_ref().map(name_of),
        default_output: default_device(&host, Kind::Output).as_ref().map(name_of),
    }
}

/// Running audio streams. Dropping it stops capture and playback.
pub struct Audio {
    _input: Stream,
    _output: Stream,
    /// Devices actually in use (may differ from the preferred ones if those failed).
    pub input_name: String,
    pub output_name: String,
    /// Why preferred/default devices were skipped on the way, if they were.
    pub notes: Vec<String>,
}

fn rate(c: &StreamConfig) -> u32 {
    c.sample_rate.0
}

fn err(what: &'static str, cfg: &StreamConfig, e: impl std::fmt::Display) -> AudioError {
    AudioError::Cpal {
        what,
        detail: format!("{} Hz, {} ch", rate(cfg), cfg.channels),
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
        cfg,
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
        cfg,
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

#[derive(Clone, Copy)]
enum Kind {
    Input,
    Output,
}

fn default_device(host: &cpal::Host, kind: Kind) -> Option<Device> {
    match kind {
        Kind::Input => host.default_input_device(),
        Kind::Output => host.default_output_device(),
    }
}

fn all_devices(host: &cpal::Host, kind: Kind) -> Vec<Device> {
    match kind {
        Kind::Input => host.input_devices().map(|d| d.collect()).unwrap_or_default(),
        Kind::Output => host.output_devices().map(|d| d.collect()).unwrap_or_default(),
    }
}

fn name_of(d: &Device) -> String {
    d.name().unwrap_or_else(|_| "unknown device".into())
}

/// Open a stream: first the device the user picked (if any), then the system default, then every
/// other device. Windows can need a moment (or a different endpoint) right after another stream was
/// opened, e.g. when a Bluetooth headset switches to its hands-free profile as soon as the
/// microphone starts, so the default device is retried with a fresh lookup each time.
/// Returns the stream plus the reasons earlier candidates were skipped.
fn open_with_fallback(
    what: &'static str,
    kind: Kind,
    preferred: Option<&str>,
    mut open: impl FnMut(&Device) -> Result<Stream, AudioError>,
) -> Result<(Stream, Vec<String>), AudioError> {
    let host = cpal::default_host();
    let mut tried: Vec<String> = Vec::new();

    if let Some(want) = preferred {
        match all_devices(&host, kind).into_iter().find(|d| name_of(d) == want) {
            Some(dev) => {
                for attempt in 0..2 {
                    match open(&dev) {
                        Ok(s) => return Ok((s, tried)),
                        Err(e) => {
                            tracing::warn!("{what} '{want}' attempt {}: {e}", attempt + 1);
                            if attempt == 0 {
                                tried.push(format!("'{want}' (gewählt): {e}"));
                                std::thread::sleep(std::time::Duration::from_millis(300));
                            }
                        }
                    }
                }
            }
            None => tried.push(format!("'{want}' (gewählt): nicht gefunden")),
        }
    }

    let mut default_name = None;
    for attempt in 0..3 {
        let Some(dev) = default_device(&host, kind) else {
            return Err(AudioError::NoDevice(what));
        };
        let name = name_of(&dev);
        match open(&dev) {
            Ok(s) => return Ok((s, tried)),
            Err(e) => {
                tracing::warn!("{what} '{name}' attempt {}: {e}", attempt + 1);
                if attempt == 0 {
                    tried.push(format!("'{name}' (Standard): {e}"));
                }
                default_name = Some(name);
            }
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
    for dev in all_devices(&host, kind) {
        let name = name_of(&dev);
        if Some(&name) == default_name.as_ref() || Some(name.as_str()) == preferred {
            continue;
        }
        match open(&dev) {
            Ok(s) => {
                tracing::warn!("{what}: default device failed, using '{name}'");
                return Ok((s, tried));
            }
            Err(e) => tried.push(format!("'{name}': {e}")),
        }
    }
    Err(AudioError::AllFailed { what, tried: tried.join(" | ") })
}

fn start_input(dev: &Device, shared: &Arc<Shared>, on_packet: &PacketSink, on_levels: &LevelSink) -> Result<(Stream, StreamConfig, SampleFormat), AudioError> {
    // Device format -> mono -> 48 kHz -> i16 -> Opus.
    let conf = dev.default_input_config().map_err(|e| AudioError::Cpal {
        what: "audio input",
        detail: "default config".into(),
        msg: e.to_string(),
    })?;
    let cfg: StreamConfig = conf.config();
    let fmt = conf.sample_format();
    let sh = shared.clone();
    let on_packet = on_packet.clone();
    let on_levels = on_levels.clone();
    let meter = Meter::new(SAMPLE_RATE);
    let mut last_level = std::time::Instant::now();
    let mut rs = Resampler::new(rate(&cfg), SAMPLE_RATE);
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
        if last_level.elapsed() >= LEVEL_INTERVAL {
            last_level = std::time::Instant::now();
            on_levels(Level { tx: true, bands: meter.analyze(&pcm) });
        }
        if let Ok(packets) = tx.push_pcm(&pcm) {
            for p in packets {
                on_packet(p);
            }
        }
    };
    let stream = match fmt {
        SampleFormat::I16 => build_input::<i16>(dev, &cfg, on_mono),
        SampleFormat::I32 => build_input::<i32>(dev, &cfg, on_mono),
        SampleFormat::U16 => build_input::<u16>(dev, &cfg, on_mono),
        SampleFormat::F32 => build_input::<f32>(dev, &cfg, on_mono),
        format => return Err(AudioError::Unsupported { what: "audio input", format }),
    }?;
    stream.play().map_err(|e| err("audio input", &cfg, e))?;
    Ok((stream, cfg, fmt))
}

fn start_output(dev: &Device, shared: &Arc<Shared>, on_speakers: &SpeakerSink, on_levels: &LevelSink) -> Result<(Stream, StreamConfig, SampleFormat), AudioError> {
    // Mixer (48 kHz mono) -> device rate -> all channels.
    let conf = dev.default_output_config().map_err(|e| AudioError::Cpal {
        what: "audio output",
        detail: "default config".into(),
        msg: e.to_string(),
    })?;
    let cfg: StreamConfig = conf.config();
    let fmt = conf.sample_format();
    let sh = shared.clone();
    let on_speakers = on_speakers.clone();
    let on_levels = on_levels.clone();
    let meter = Meter::new(SAMPLE_RATE);
    let mut last_level = std::time::Instant::now();
    let mut rs = Resampler::new(SAMPLE_RATE, rate(&cfg));
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
            let heard = !info.speaking.is_empty();
            if info.speaking != last_speaking || !info.ended.is_empty() {
                last_speaking = info.speaking.clone();
                on_speakers(info);
            }
            // Only while there is something to show; the display decays on its own afterwards.
            if heard && last_level.elapsed() >= LEVEL_INTERVAL {
                last_level = std::time::Instant::now();
                on_levels(Level { tx: false, bands: meter.analyze(&frame) });
            }
            src.clear();
            src.extend(frame.iter().map(|&s| s as f32 / 32768.0));
            conv.clear();
            rs.process(&src, &mut conv);
            ready.extend(conv.iter().copied());
        }
        ready.pop_front().unwrap_or(0.0)
    };
    let stream = match fmt {
        SampleFormat::I16 => build_output::<i16>(dev, &cfg, next),
        SampleFormat::I32 => build_output::<i32>(dev, &cfg, next),
        SampleFormat::U16 => build_output::<u16>(dev, &cfg, next),
        SampleFormat::F32 => build_output::<f32>(dev, &cfg, next),
        format => return Err(AudioError::Unsupported { what: "audio output", format }),
    }?;
    stream.play().map_err(|e| err("audio output", &cfg, e))?;
    Ok((stream, cfg, fmt))
}

impl Audio {
    pub fn start(
        shared: Arc<Shared>,
        on_packet: PacketSink,
        on_speakers: SpeakerSink,
        on_levels: LevelSink,
        prefs: DevicePrefs,
    ) -> Result<Self, AudioError> {
        let mut in_info = None;
        let (input, mut notes) = open_with_fallback("audio input", Kind::Input, prefs.input.as_deref(), |d| {
            let (s, c, f) = start_input(d, &shared, &on_packet, &on_levels)?;
            in_info = Some((name_of(d), rate(&c), c.channels, f));
            Ok(s)
        })?;
        // Opening the microphone can make Windows re-route the output (Bluetooth hands-free
        // profile), so give it a moment and look the output device up afresh.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut out_info = None;
        let (output, out_notes) = open_with_fallback("audio output", Kind::Output, prefs.output.as_deref(), |d| {
            let (s, c, f) = start_output(d, &shared, &on_speakers, &on_levels)?;
            out_info = Some((name_of(d), rate(&c), c.channels, f));
            Ok(s)
        })?;
        notes.extend(out_notes);
        tracing::info!("audio: input {in_info:?}, output {out_info:?}");
        Ok(Self {
            _input: input,
            _output: output,
            input_name: in_info.map(|i| i.0).unwrap_or_default(),
            output_name: out_info.map(|o| o.0).unwrap_or_default(),
            notes,
        })
    }
}
