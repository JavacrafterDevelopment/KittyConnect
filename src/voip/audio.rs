//! Microphone in, speakers out, the way Discord does it: 48 kHz Opus.
//!
//! Send path, every 10 ms block of microphone audio:
//!   device rate → 48 kHz (windowed-sinc resampler, no aliasing)
//!   → 80 Hz high-pass (rumble, desk knocks, DC)
//!   → input volume (the user's setting)
//!   → noise suppression (RNNoise, which also says whether it's speech)
//!   → voice gate: automatic (speech detected) or a manual dB threshold,
//!     held open 400 ms after speech and faded, never hard-cut
//!   → soft limiter → Opus 64 kbit/s, 20 ms frames, in-band FEC
//!
//! Packets go out as UDP datagrams (see `media`), so a lost one costs only
//! itself.
//!
//! Receive path: a small jitter buffer per person that puts packets back in
//! order (UDP can reorder), then one Opus decoder per person, which rebuilds a
//! lost frame from the next one's FEC or conceals it, per-person volume, then
//! the mix is resampled to the speakers' rate.
//!
//! There's no echo cancellation: use headphones, or the mic picks up the call.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use opus1::{Application, Bitrate, Channels, InbandFec, OpusDecoder, OpusEncoder, SampleRate};

use super::media;

pub const RATE: u32 = 48_000;
/// One Opus packet: 20 ms.
pub const FRAME: usize = 960;
/// RNNoise works on 10 ms blocks.
const BLOCK: usize = nnnoiseless::DenoiseState::FRAME_SIZE;
const BITRATE: u32 = 64_000;
/// Keep sending this many 10 ms blocks after speech stops.
const HANGOVER: u32 = 40;
/// Wait for this many of a speaker's packets (60 ms) before playing, to ride out jitter.
const PRIME: usize = 3;
/// More waiting than 400 ms and the oldest are dropped, back to 100 ms.
const MAX_PENDING: usize = 20;
const TRIM_TO: usize = 5;
/// A jump in sequence numbers bigger than this isn't concealed; playback
/// just carries on from the new packet.
const MAX_GAP: u32 = 10;
/// Conceal at most this many missing frames before going quiet.
const MAX_PLC: u32 = 3;

fn f32_atomic(v: f32) -> AtomicU32 {
    AtomicU32::new(v.to_bits())
}

/// The voice settings from the gear menu. Shared with the audio callbacks, so
/// a change applies at once.
pub struct Settings {
    /// Microphone volume, 1.0 = as recorded, up to 4.0.
    input_gain: AtomicU32,
    /// Let speech detection decide when you're talking.
    auto_sensitivity: AtomicBool,
    /// With automatic off: send when the level is above this, in dBFS.
    threshold_db: AtomicU32,
    noise_suppression: AtomicBool,
    /// Everyone else's volume, 1.0 = normal, up to 2.0.
    output_gain: AtomicU32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            input_gain: f32_atomic(1.0),
            auto_sensitivity: AtomicBool::new(true),
            threshold_db: f32_atomic(-50.0),
            noise_suppression: AtomicBool::new(true),
            output_gain: f32_atomic(1.0),
        }
    }
}

impl Settings {
    pub fn set(&self, input_gain: f32, auto: bool, threshold_db: f32, noise_suppression: bool, output_gain: f32) {
        self.input_gain.store(input_gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        self.auto_sensitivity.store(auto, Ordering::Relaxed);
        self.threshold_db.store(threshold_db.clamp(-100.0, 0.0).to_bits(), Ordering::Relaxed);
        self.noise_suppression.store(noise_suppression, Ordering::Relaxed);
        self.output_gain.store(output_gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    fn get(a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }
}

// --------------------------------------------------------------- resampler

/// A windowed-sinc resampler (Blackman window, 16 taps a side). When going
/// down in rate it low-passes at the new Nyquist, so nothing folds back as
/// the metallic aliasing a straight interpolation gives.
pub struct Resampler {
    step: f64,
    cutoff: f64,
    pos: f64,
    buf: Vec<f32>,
}

const TAPS: usize = 16;

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let step = from as f64 / to as f64;
        Resampler { step, cutoff: (1.0 / step).min(1.0) * 0.95, pos: TAPS as f64, buf: vec![0.0; TAPS] }
    }

    pub fn passthrough(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough() {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        while self.pos + (TAPS as f64) < self.buf.len() as f64 {
            let center = self.pos.floor() as isize;
            let frac = self.pos - center as f64;
            let mut acc = 0.0f64;
            let mut norm = 0.0f64;
            for k in -(TAPS as isize) + 1..=TAPS as isize {
                let x = k as f64 - frac;
                let w = blackman(x / TAPS as f64);
                let s = sinc(x * self.cutoff) * self.cutoff * w;
                acc += self.buf[(center + k) as usize] as f64 * s;
                norm += s;
            }
            out.push((acc / norm.max(1e-9)) as f32);
            self.pos += self.step;
        }
        // Keep the history the next call needs.
        let keep_from = (self.pos.floor() as usize).saturating_sub(TAPS);
        self.buf.drain(..keep_from);
        self.pos -= keep_from as f64;
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

fn blackman(x: f64) -> f64 {
    // x in [-1, 1]
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let t = std::f64::consts::PI * (x + 1.0);
    0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos()
}

/// A second-order high-pass (RBJ biquad) at 80 Hz.
struct HighPass {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl HighPass {
    fn new(rate: f32, freq: f32) -> HighPass {
        let w = 2.0 * std::f32::consts::PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        HighPass {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn run(&mut self, s: f32) -> f32 {
        let y = self.b[0] * s + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [s, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

fn db(rms: f32) -> f32 {
    (20.0 * rms.max(1e-5).log10()).max(-100.0)
}

/// Rounds off peaks instead of clipping them.
fn soft_limit(s: f32) -> f32 {
    if s.abs() < 0.9 {
        s
    } else {
        s.signum() * (0.9 + 0.1 * ((s.abs() - 0.9) / 0.1).tanh())
    }
}

// ------------------------------------------------------------------- mixer

struct Voice {
    decoder: OpusDecoder,
    /// Packets waiting to be played, by sequence number.
    pending: BTreeMap<u32, Vec<u8>>,
    /// The next sequence number to play, once playing.
    next: Option<u32>,
    /// Decoded audio ready for the speakers.
    buf: VecDeque<f32>,
    plc: u32,
    loud: Option<Instant>,
}

/// Everyone else's voice: queued per person, decoded in order, and mixed for the speakers.
pub struct Mixer {
    voices: Mutex<HashMap<u16, Voice>>,
    /// Per-person volume from the right-click menu (only affects this PC).
    gains: Mutex<HashMap<u16, f32>>,
    deaf: AtomicBool,
    pub settings: Arc<Settings>,
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer {
            voices: Mutex::default(),
            gains: Mutex::default(),
            deaf: AtomicBool::new(false),
            settings: Arc::new(Settings::default()),
        }
    }
}

impl Voice {
    /// Decodes the next packet into `buf`: the packet itself, or if it's
    /// missing, a rebuild from the following packet's FEC copy, or
    /// concealment. False once there's nothing left to play.
    fn decode_next(&mut self) -> bool {
        let Some(n) = self.next else { return false };
        let mut pcm = [0f32; FRAME];
        if let Some(p) = self.pending.remove(&n) {
            self.plc = 0;
            if let Ok(k) = self.decoder.decode(&p, FRAME, &mut pcm) {
                let rms = (pcm[..k].iter().map(|s| s * s).sum::<f32>() / k.max(1) as f32).sqrt();
                if rms > 0.01 {
                    self.loud = Some(Instant::now());
                }
                self.buf.extend(pcm[..k].iter());
            }
        } else if let Some((&later, _)) = self.pending.iter().next() {
            // Lost in the middle of a sentence.
            if later.wrapping_sub(n) > MAX_GAP {
                self.next = Some(later);
                return true;
            }
            let fec = if later == n.wrapping_add(1) { self.pending.get(&later).cloned() } else { None };
            let ok = match fec {
                Some(p) => self.decoder.decode_fec(&p, FRAME, &mut pcm).is_ok(),
                None => self.decoder.decode(&[], FRAME, &mut pcm).is_ok(),
            };
            if ok {
                self.buf.extend(pcm.iter());
            }
        } else {
            // Ran dry: conceal a little, then go quiet until they speak again.
            if self.plc >= MAX_PLC {
                self.next = None;
                return false;
            }
            self.plc += 1;
            if self.decoder.decode(&[], FRAME, &mut pcm).is_ok() {
                self.buf.extend(pcm.iter());
            }
        }
        self.next = Some(n.wrapping_add(1));
        true
    }
}

impl Mixer {
    pub fn with_settings(settings: Arc<Settings>) -> Mixer {
        Mixer { settings, ..Mixer::default() }
    }

    pub fn set_gain(&self, member: u16, gain: f32) {
        self.gains.lock().unwrap().insert(member, gain.clamp(0.0, 2.0));
    }

    /// One Opus packet from `from`. Packets are held briefly and played in
    /// sequence order, so one that arrives out of order is still used.
    pub fn push_packet(&self, from: u16, seq: u32, packet: &[u8]) {
        let mut voices = self.voices.lock().unwrap();
        let v = match voices.entry(from) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(decoder) = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono) else { return };
                e.insert(Voice { decoder, pending: BTreeMap::new(), next: None, buf: VecDeque::new(), plc: 0, loud: None })
            }
        };
        if let Some(n) = v.next {
            if (seq.wrapping_sub(n) as i32) < 0 {
                return; // too late: that moment has been played
            }
        }
        v.pending.insert(seq, packet.to_vec());
        // Too far behind (a stall, then a burst): skip ahead.
        while v.pending.len() > MAX_PENDING {
            let keep: Vec<u32> = v.pending.keys().rev().take(TRIM_TO).copied().collect();
            let first = *keep.last().unwrap();
            v.pending.retain(|s, _| *s >= first);
            if v.next.is_some() {
                v.next = Some(first);
                v.buf.clear();
            }
        }
    }

    /// Fills `out` with the mix at 48 kHz.
    fn pull(&self, out: &mut [f32]) {
        out.fill(0.0);
        if self.deaf.load(Ordering::Relaxed) {
            return;
        }
        let gains = self.gains.lock().unwrap().clone();
        let master = Settings::get(&self.settings.output_gain);
        let mut voices = self.voices.lock().unwrap();
        for (id, v) in voices.iter_mut() {
            if v.next.is_none() {
                if v.pending.len() < PRIME {
                    continue;
                }
                v.next = v.pending.keys().next().copied();
                v.plc = 0;
            }
            while v.buf.len() < out.len() {
                if !v.decode_next() {
                    break;
                }
            }
            let gain = gains.get(id).copied().unwrap_or(1.0) * master;
            for o in out.iter_mut() {
                match v.buf.pop_front() {
                    Some(s) => *o += s * gain,
                    None => break,
                }
            }
        }
        for o in out.iter_mut() {
            *o = soft_limit(*o);
        }
    }

    /// Members heard in the last third of a second.
    pub fn speaking(&self) -> Vec<u16> {
        let voices = self.voices.lock().unwrap();
        let mut ids: Vec<u16> =
            voices.iter().filter(|(_, v)| v.loud.is_some_and(|t| t.elapsed() < Duration::from_millis(350))).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    }

    pub fn set_deaf(&self, deaf: bool) {
        self.deaf.store(deaf, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.voices.lock().unwrap().clear();
    }
}

// ------------------------------------------------------------------ engine

/// The running microphone and speaker streams. Dropping it stops both.
pub struct Engine {
    stop: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    /// The mic's level in dBFS (-100..0), after input volume, as f32 bits.
    pub level: Arc<AtomicU32>,
    /// True while the gate is open and we're sending.
    pub talking: Arc<AtomicBool>,
    pub mic_name: String,
    pub speaker_name: String,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Engine {
    /// Opens the default microphone and speakers. Packets from the mic go out
    /// on `out` (the room's UDP). A missing mic is not fatal: you can still
    /// listen. A missing output device is.
    pub fn start(mixer: Arc<Mixer>, out: media::Sender, muted: bool) -> Result<Engine, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(muted));
        let level = Arc::new(f32_atomic(-100.0));
        let talking = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = channel::<Result<(String, String), String>>();

        let (s, m, l, t) = (stop.clone(), muted.clone(), level.clone(), talking.clone());
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let Some(speaker) = host.default_output_device() else {
                let _ = ready_tx.send(Err("No speakers or headphones found.".into()));
                return;
            };
            let settings = mixer.settings.clone();
            let output = match build_output(&speaker, mixer) {
                Ok(o) => o,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("Couldn't open the speakers: {e}")));
                    return;
                }
            };
            let mic = host.default_input_device();
            let sink: Sink = Box::new(move |d| out.urgent(d));
            let input = mic.as_ref().map(|d| Capture::new(sink, m, l, t, settings).and_then(|c| build_input(d, c)));
            let mic_name = match (&mic, &input) {
                (Some(d), Some(Ok(_))) => d.name().unwrap_or_else(|_| "Microphone".into()),
                (Some(_), Some(Err(e))) => format!("Microphone unavailable ({e})"),
                _ => "No microphone found".into(),
            };
            if output.play().is_err() {
                let _ = ready_tx.send(Err("Couldn't start the speakers.".into()));
                return;
            }
            if let Some(Ok(i)) = &input {
                let _ = i.play();
            }
            let _ = ready_tx.send(Ok((mic_name, speaker.name().unwrap_or_else(|_| "Speakers".into()))));
            // The streams live on this thread (they aren't Send) until stopped.
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(input);
            drop(output);
        });

        let (mic_name, speaker_name) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "The audio devices didn't respond.".to_string())??;
        Ok(Engine { stop, muted, level, talking, mic_name, speaker_name })
    }
}

/// Where finished voice datagrams go.
type Sink = Box<dyn Fn(Vec<u8>) + Send>;

/// The send path, from device samples to Opus packets.
struct Capture {
    channels: usize,
    resampler: Resampler,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    block: Vec<f32>,
    frame: Vec<f32>,
    highpass: HighPass,
    denoise: Box<nnnoiseless::DenoiseState<'static>>,
    denoised: Vec<f32>,
    encoder: OpusEncoder,
    packet: Vec<u8>,
    seq: u32,
    hang: u32,
    /// The gate's current gain, eased towards 0 or 1 so it never clicks.
    gate: f32,
    out: Sink,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    talking: Arc<AtomicBool>,
    settings: Arc<Settings>,
}

impl Capture {
    fn new(
        out: Sink,
        muted: Arc<AtomicBool>,
        level: Arc<AtomicU32>,
        talking: Arc<AtomicBool>,
        settings: Arc<Settings>,
    ) -> Result<Capture, String> {
        let mut encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).map_err(|e| format!("{e:?}"))?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(NonZeroU32::new(BITRATE).unwrap()));
        encoder.set_inband_fec(InbandFec::On);
        let _ = encoder.set_packet_loss_perc(10);
        let _ = encoder.set_complexity(9);
        Ok(Capture {
            channels: 1,
            resampler: Resampler::new(RATE, RATE),
            mono: Vec::new(),
            resampled: Vec::new(),
            block: Vec::with_capacity(BLOCK),
            frame: Vec::with_capacity(FRAME),
            highpass: HighPass::new(RATE as f32, 80.0),
            denoise: nnnoiseless::DenoiseState::new(),
            denoised: vec![0.0; BLOCK],
            encoder,
            packet: vec![0u8; 1500],
            seq: 0,
            hang: 0,
            gate: 0.0,
            out,
            muted,
            level,
            talking,
            settings,
        })
    }

    fn feed(&mut self, data: &[f32]) {
        self.mono.clear();
        for chunk in data.chunks(self.channels) {
            self.mono.push(chunk.iter().sum::<f32>() / chunk.len() as f32);
        }
        self.resampled.clear();
        let mono = std::mem::take(&mut self.mono);
        self.resampler.process(&mono, &mut self.resampled);
        self.mono = mono;
        for i in 0..self.resampled.len() {
            self.block.push(self.resampled[i]);
            if self.block.len() == BLOCK {
                self.finish_block();
            }
        }
    }

    /// One 10 ms block: filter, gain, suppress, gate, and every second block an Opus packet.
    fn finish_block(&mut self) {
        let gain = Settings::get(&self.settings.input_gain);
        for s in self.block.iter_mut() {
            *s = self.highpass.run(*s) * gain;
        }
        let rms = (self.block.iter().map(|s| s * s).sum::<f32>() / BLOCK as f32).sqrt();
        let level = db(rms);
        // The meter rises at once and falls 1.5 dB per block, so it's readable.
        let prev = f32::from_bits(self.level.load(Ordering::Relaxed));
        self.level.store(level.max(prev - 1.5).to_bits(), Ordering::Relaxed);

        // RNNoise wants samples on the 16-bit scale.
        let scaled: Vec<f32> = self.block.iter().map(|s| s * 32768.0).collect();
        let vad = self.denoise.process_frame(&mut self.denoised, &scaled);
        let ns = self.settings.noise_suppression.load(Ordering::Relaxed);

        let speaking = if self.settings.auto_sensitivity.load(Ordering::Relaxed) {
            vad > 0.5 && level > -65.0
        } else {
            level > Settings::get(&self.settings.threshold_db)
        };
        if speaking {
            self.hang = HANGOVER;
        } else {
            self.hang = self.hang.saturating_sub(1);
        }
        let open = self.hang > 0 && !self.muted.load(Ordering::Relaxed);
        self.talking.store(open, Ordering::Relaxed);

        // Ease the gate: about 5 ms to open, 60 ms to close.
        let target = if open { 1.0 } else { 0.0 };
        let rate = if open { 1.0 / 240.0 } else { 1.0 / 2880.0 };
        for i in 0..BLOCK {
            let s = if ns { self.denoised[i] / 32768.0 } else { self.block[i] };
            self.gate += (target - self.gate).clamp(-rate, rate);
            self.frame.push(soft_limit(s * self.gate));
        }
        self.block.clear();

        if self.frame.len() == FRAME {
            if self.gate > 0.0 || open {
                if let Ok(n) = self.encoder.encode(&self.frame, FRAME, &mut self.packet) {
                    (self.out)(media::voice(self.seq, &self.packet[..n]));
                    self.seq = self.seq.wrapping_add(1);
                }
            }
            self.frame.clear();
        }
    }
}

fn build_input(device: &cpal::Device, mut cap: Capture) -> Result<cpal::Stream, String> {
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    cap.channels = config.channels().max(1) as usize;
    cap.resampler = Resampler::new(config.sample_rate().0, RATE);
    let stream_config: cpal::StreamConfig = config.clone().into();
    match config.sample_format() {
        SampleFormat::F32 => input_stream::<f32>(device, &stream_config, cap),
        SampleFormat::I16 => input_stream::<i16>(device, &stream_config, cap),
        SampleFormat::U16 => input_stream::<u16>(device, &stream_config, cap),
        SampleFormat::I32 => input_stream::<i32>(device, &stream_config, cap),
        other => Err(format!("unsupported mic format {other:?}")),
    }
}

fn input_stream<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut cap: Capture) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                scratch.clear();
                scratch.extend(data.iter().map(|s| f32::from_sample_(*s)));
                cap.feed(&scratch);
            },
            |_| {},
            None,
        )
        .map_err(|e| e.to_string())
}

/// The 48 kHz mix, resampled to the device rate and copied to every channel.
struct Playback {
    mixer: Arc<Mixer>,
    channels: usize,
    resampler: Resampler,
    chunk: Vec<f32>,
    ready: VecDeque<f32>,
    scratch: Vec<f32>,
}

impl Playback {
    fn next_sample(&mut self) -> f32 {
        while self.ready.is_empty() {
            self.mixer.pull(&mut self.chunk);
            self.scratch.clear();
            self.resampler.process(&self.chunk, &mut self.scratch);
            self.ready.extend(self.scratch.iter());
        }
        self.ready.pop_front().unwrap_or(0.0)
    }
}

fn build_output(device: &cpal::Device, mixer: Arc<Mixer>) -> Result<cpal::Stream, String> {
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let pb = Playback {
        mixer,
        channels: config.channels().max(1) as usize,
        resampler: Resampler::new(RATE, config.sample_rate().0),
        chunk: vec![0.0; 480],
        ready: VecDeque::new(),
        scratch: Vec::new(),
    };
    let stream_config: cpal::StreamConfig = config.clone().into();
    match config.sample_format() {
        SampleFormat::F32 => output_stream::<f32>(device, &stream_config, pb),
        SampleFormat::I16 => output_stream::<i16>(device, &stream_config, pb),
        SampleFormat::U16 => output_stream::<u16>(device, &stream_config, pb),
        SampleFormat::I32 => output_stream::<i32>(device, &stream_config, pb),
        other => Err(format!("unsupported speaker format {other:?}")),
    }
}

fn output_stream<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut pb: Playback) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let ch = pb.channels;
                for frame in data.chunks_mut(ch) {
                    let v = T::from_sample_(pb.next_sample());
                    for s in frame {
                        *s = v;
                    }
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(settings: Arc<Settings>) -> (Capture, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let cap = Capture::new(
            Box::new(move |d| {
                let _ = tx.try_send(d);
            }),
            Arc::new(AtomicBool::new(false)),
            Arc::new(f32_atomic(-100.0)),
            Arc::new(AtomicBool::new(false)),
            settings,
        )
        .unwrap();
        (cap, rx)
    }

    fn tone(rate: u32, ms: u32, freq: f32, amp: f32) -> Vec<f32> {
        (0..rate * ms / 1000).map(|i| (i as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * amp).collect()
    }

    #[test]
    fn resampler_keeps_a_tone_and_its_level() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        r.process(&tone(44_100, 200, 1000.0, 0.5), &mut out);
        assert!((out.len() as i32 - 9600).abs() < 40, "{} samples", out.len());
        let rms = (out[1000..8000].iter().map(|s| s * s).sum::<f32>() / 7000.0).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn resampler_removes_what_cannot_fit() {
        // 20 kHz can't exist at 16 kHz; a plain interpolator would fold it to 4 kHz.
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        r.process(&tone(48_000, 200, 20_000.0, 0.5), &mut out);
        let rms = (out[200..3000].iter().map(|s| s * s).sum::<f32>() / 2800.0).sqrt();
        assert!(rms < 0.02, "aliasing left {rms}");
    }

    /// Manual threshold: a tone above it is sent as Opus, silence isn't.
    #[test]
    fn manual_gate_sends_speech_level_only() {
        let s = Arc::new(Settings::default());
        s.set(1.0, false, -40.0, false, 1.0);
        let (mut cap, rx) = capture(s.clone());
        cap.feed(&vec![0.0; 9600]);
        assert_eq!(rx.try_iter().count(), 0, "silence sends nothing");
        cap.feed(&tone(48_000, 200, 300.0, 0.3));
        let packets: Vec<_> = rx.try_iter().collect();
        assert!(packets.len() >= 9, "got {}", packets.len());
        // A quiet mic below the threshold isn't sent; raising input volume fixes that.
        let (mut quiet, rx2) = capture(s.clone());
        quiet.feed(&tone(48_000, 200, 300.0, 0.004));
        assert_eq!(rx2.try_iter().count(), 0);
        s.set(4.0, false, -45.0, false, 1.0);
        quiet.feed(&tone(48_000, 200, 300.0, 0.004));
        assert!(rx2.try_iter().count() > 0, "input volume lifts a quiet mic over the threshold");
    }

    /// Opus out of the capture decodes in the mixer; a lost packet is concealed.
    #[test]
    fn opus_round_trip_with_a_lost_packet() {
        let s = Arc::new(Settings::default());
        s.set(1.0, false, -60.0, false, 1.0);
        let (mut cap, rx) = capture(s);
        cap.feed(&tone(48_000, 300, 440.0, 0.3));
        let packets: Vec<Vec<u8>> = rx
            .try_iter()
            .map(|d| {
                assert_eq!(d[0], media::VOICE);
                d[1 + 4..].to_vec()
            })
            .collect();
        assert!(packets.len() >= 12);
        assert!(packets.iter().all(|p| p.len() < 400), "64 kbit/s is about 160 bytes a packet");
        let m = Mixer::default();
        for (seq, p) in packets.iter().enumerate() {
            if seq == 5 {
                continue; // lost on the way
            }
            m.push_packet(4, seq as u32, p);
        }
        let mut out = vec![0f32; FRAME * packets.len()];
        m.pull(&mut out);
        let rms = (out[FRAME * 3..FRAME * 10].iter().map(|s| s * s).sum::<f32>() / (FRAME * 7) as f32).sqrt();
        assert!(rms > 0.1, "decoded level {rms}");
        assert_eq!(m.speaking(), vec![4]);
        // Their volume at 50% for me.
        m.set_gain(4, 0.0);
        m.push_packet(4, 100, &packets[2]);
        let mut quiet = vec![0f32; FRAME];
        m.pull(&mut quiet);
        assert!(quiet.iter().all(|s| s.abs() < 1e-6));
    }

    /// UDP can deliver packets out of order; the jitter buffer puts them back,
    /// so a swapped pair plays exactly as if it had arrived in order.
    #[test]
    fn reordered_packets_play_in_order() {
        let s = Arc::new(Settings::default());
        s.set(1.0, false, -60.0, false, 1.0);
        let (mut cap, rx) = capture(s);
        cap.feed(&tone(48_000, 300, 440.0, 0.3));
        let packets: Vec<Vec<u8>> = rx.try_iter().map(|d| d[5..].to_vec()).collect();
        let play = |order: &[usize]| {
            let m = Mixer::default();
            for &i in order {
                m.push_packet(1, i as u32, &packets[i]);
            }
            let mut out = vec![0f32; FRAME * 10];
            m.pull(&mut out);
            out
        };
        let in_order: Vec<usize> = (0..12).collect();
        let mut swapped = in_order.clone();
        swapped.swap(4, 5);
        assert_eq!(play(&in_order), play(&swapped));
        // And a packet older than what's already played is let go.
        let m = Mixer::default();
        for i in 0..6 {
            m.push_packet(1, i, &packets[i as usize]);
        }
        let mut out = vec![0f32; FRAME * 6];
        m.pull(&mut out);
        m.push_packet(1, 2, &packets[2]);
        assert!(m.voices.lock().unwrap()[&1].pending.is_empty(), "too late to play");
    }
}
