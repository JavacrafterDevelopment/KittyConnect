//! Microphone in, speakers out. Voice travels as 16 kHz mono in 20 ms frames
//! (320 samples), ADPCM-encoded. Everything here runs on the audio thread and
//! the device callbacks; the network side only sees encoded frames.
//!
//! There's no echo cancellation: use headphones, or the mic picks up the call.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use super::adpcm;
use super::wire::{self, Out};

pub const RATE: u32 = 16_000;
pub const FRAME: usize = 320;
/// Below this RMS the mic is treated as silent and nothing is sent.
const GATE: f32 = 0.012;
/// Keep sending this many frames after speech stops, so word endings aren't cut.
const HANGOVER: u32 = 15;
/// Wait for this much of a speaker's audio before playing, to ride out jitter.
const PRIME: usize = 960;
/// More queued than this and the oldest is dropped, to keep delay down.
const MAX_QUEUE: usize = 6400;
const TRIM_TO: usize = 1600;

#[derive(Default)]
struct Jitter {
    buf: VecDeque<i16>,
    primed: bool,
}

/// Everyone else's voice, queued per speaker and mixed for the speakers.
#[derive(Default)]
pub struct Mixer {
    streams: Mutex<HashMap<u16, Jitter>>,
    loud: Mutex<HashMap<u16, Instant>>,
    /// Per-person volume set from the right-click menu (1.0 = normal, 0 = muted
    /// for me). Only affects what this PC plays.
    gains: Mutex<HashMap<u16, f32>>,
    deaf: AtomicBool,
}

impl Mixer {
    pub fn set_gain(&self, member: u16, gain: f32) {
        self.gains.lock().unwrap().insert(member, gain.clamp(0.0, 2.0));
    }

    pub fn push(&self, from: u16, samples: &[i16]) {
        let rms = (samples.iter().map(|s| (*s as f32 / 32768.0).powi(2)).sum::<f32>() / samples.len().max(1) as f32).sqrt();
        if rms > GATE {
            self.loud.lock().unwrap().insert(from, Instant::now());
        }
        let mut streams = self.streams.lock().unwrap();
        let j = streams.entry(from).or_default();
        j.buf.extend(samples);
        if j.buf.len() > MAX_QUEUE {
            let extra = j.buf.len() - TRIM_TO;
            j.buf.drain(..extra);
        }
    }

    /// Fills `out` with the mix at 16 kHz.
    fn pull(&self, out: &mut [i16]) {
        let mut acc = vec![0i32; out.len()];
        {
            let gains = self.gains.lock().unwrap().clone();
            let mut streams = self.streams.lock().unwrap();
            for (id, j) in streams.iter_mut() {
                if !j.primed && j.buf.len() >= PRIME {
                    j.primed = true;
                }
                if !j.primed {
                    continue;
                }
                let gain = gains.get(id).copied().unwrap_or(1.0);
                for a in acc.iter_mut() {
                    match j.buf.pop_front() {
                        Some(s) => *a += (s as f32 * gain) as i32,
                        None => {
                            j.primed = false;
                            break;
                        }
                    }
                }
            }
        }
        let deaf = self.deaf.load(Ordering::Relaxed);
        for (o, a) in out.iter_mut().zip(acc) {
            *o = if deaf { 0 } else { a.clamp(-32768, 32767) as i16 };
        }
    }

    /// Members heard in the last third of a second.
    pub fn speaking(&self) -> Vec<u16> {
        let loud = self.loud.lock().unwrap();
        let mut ids: Vec<u16> = loud.iter().filter(|(_, t)| t.elapsed() < Duration::from_millis(350)).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    }

    pub fn set_deaf(&self, deaf: bool) {
        self.deaf.store(deaf, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.streams.lock().unwrap().clear();
        self.loud.lock().unwrap().clear();
    }
}

/// The running microphone and speaker streams. Dropping it stops both.
pub struct Engine {
    stop: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    /// The mic's current level, 0..1, as f32 bits.
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
    /// Opens the default microphone and speakers. Frames from the mic go to
    /// `out` (the room connection). A missing mic is not fatal: you can still
    /// listen. A missing output device is.
    pub fn start(mixer: Arc<Mixer>, out: SyncSender<Out>, muted: bool) -> Result<Engine, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(muted));
        let level = Arc::new(AtomicU32::new(0));
        let talking = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = channel::<Result<(String, String), String>>();

        let (s, m, l, t) = (stop.clone(), muted.clone(), level.clone(), talking.clone());
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let Some(speaker) = host.default_output_device() else {
                let _ = ready_tx.send(Err("No speakers or headphones found.".into()));
                return;
            };
            let output = match build_output(&speaker, mixer) {
                Ok(o) => o,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("Couldn't open the speakers: {e}")));
                    return;
                }
            };
            let mic = host.default_input_device();
            let input = mic.as_ref().map(|d| build_input(d, out, m, l, t));
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

/// Downmixes to mono and resamples to 16 kHz, then cuts 20 ms frames.
struct Capture {
    channels: usize,
    step: f64,
    pos: f64,
    src: Vec<f32>,
    smooth: bool,
    frame: Vec<i16>,
    enc: adpcm::Encoder,
    seq: u32,
    hang: u32,
    out: SyncSender<Out>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    talking: Arc<AtomicBool>,
}

impl Capture {
    fn feed(&mut self, data: &[f32]) {
        for chunk in data.chunks(self.channels) {
            self.src.push(chunk.iter().sum::<f32>() / chunk.len() as f32);
        }
        if self.smooth && self.src.len() >= 3 {
            // A gentle low-pass before decimating, against aliasing.
            for i in (2..self.src.len()).rev() {
                self.src[i] = (self.src[i] + self.src[i - 1] + self.src[i - 2]) / 3.0;
            }
        }
        while self.pos + 1.0 < self.src.len() as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            let v = self.src[i] + (self.src[i + 1] - self.src[i]) * f;
            self.frame.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            self.pos += self.step;
            if self.frame.len() == FRAME {
                self.finish_frame();
            }
        }
        let used = (self.pos as usize).min(self.src.len().saturating_sub(1));
        self.src.drain(..used);
        self.pos -= used as f64;
    }

    fn finish_frame(&mut self) {
        let rms = (self.frame.iter().map(|s| (*s as f32 / 32768.0).powi(2)).sum::<f32>() / FRAME as f32).sqrt();
        let shown = (rms * 4.0).min(1.0);
        let prev = f32::from_bits(self.level.load(Ordering::Relaxed));
        self.level.store((shown.max(prev * 0.8)).to_bits(), Ordering::Relaxed);

        if rms > GATE {
            self.hang = HANGOVER;
        } else {
            self.hang = self.hang.saturating_sub(1);
        }
        let open = self.hang > 0 && !self.muted.load(Ordering::Relaxed);
        self.talking.store(open, Ordering::Relaxed);
        if open {
            let encoded = self.enc.encode(&self.frame);
            let _ = self.out.try_send(Out::Frame(wire::audio(&self.seq.to_be_bytes(), &encoded)));
            self.seq = self.seq.wrapping_add(1);
        }
        self.frame.clear();
    }
}

fn build_input(
    device: &cpal::Device,
    out: SyncSender<Out>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    talking: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let rate = config.sample_rate().0;
    let cap = Capture {
        channels: config.channels().max(1) as usize,
        step: rate as f64 / RATE as f64,
        pos: 0.0,
        src: Vec::new(),
        smooth: rate > 2 * RATE,
        frame: Vec::with_capacity(FRAME),
        enc: adpcm::Encoder::default(),
        seq: 0,
        hang: 0,
        out,
        muted,
        level,
        talking,
    };
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

/// Resamples the 16 kHz mix up to the device rate and copies it to every channel.
struct Playback {
    mixer: Arc<Mixer>,
    channels: usize,
    step: f64,
    frac: f64,
    prev: f32,
    next: f32,
    chunk: Vec<i16>,
    at: usize,
}

impl Playback {
    fn next_sample(&mut self) -> f32 {
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            self.prev = self.next;
            if self.at >= self.chunk.len() {
                self.mixer.pull(&mut self.chunk);
                self.at = 0;
            }
            self.next = self.chunk[self.at] as f32 / 32768.0;
            self.at += 1;
        }
        let v = self.prev + (self.next - self.prev) * self.frac as f32;
        self.frac += self.step;
        v
    }
}

fn build_output(device: &cpal::Device, mixer: Arc<Mixer>) -> Result<cpal::Stream, String> {
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let pb = Playback {
        mixer,
        channels: config.channels().max(1) as usize,
        step: RATE as f64 / config.sample_rate().0 as f64,
        frac: 1.0,
        prev: 0.0,
        next: 0.0,
        chunk: vec![0; 160],
        at: 160,
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

    #[test]
    fn capture_cuts_16k_frames_from_48k_stereo() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut cap = Capture {
            channels: 2,
            step: 3.0,
            pos: 0.0,
            src: Vec::new(),
            smooth: true,
            frame: Vec::new(),
            enc: adpcm::Encoder::default(),
            seq: 0,
            hang: 0,
            out: tx,
            muted: Arc::new(AtomicBool::new(false)),
            level: Arc::new(AtomicU32::new(0)),
            talking: Arc::new(AtomicBool::new(false)),
        };
        // 100 ms of a loud 440 Hz tone, 48 kHz stereo.
        let data: Vec<f32> = (0..4800).flat_map(|i| {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            [v, v]
        }).collect();
        cap.feed(&data);
        let frames: Vec<_> = rx.try_iter().collect();
        // 100 ms at 16 kHz is 1600 samples: five 20 ms frames (the last may still be filling).
        assert!(frames.len() >= 4, "got {} frames", frames.len());
        assert!(cap.level.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn silence_sends_nothing() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut cap = Capture {
            channels: 1, step: 1.0, pos: 0.0, src: Vec::new(), smooth: false, frame: Vec::new(),
            enc: adpcm::Encoder::default(), seq: 0, hang: 0, out: tx,
            muted: Arc::new(AtomicBool::new(false)), level: Arc::new(AtomicU32::new(0)), talking: Arc::new(AtomicBool::new(false)),
        };
        cap.feed(&vec![0.0; 3200]);
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn mixer_waits_to_prime_then_plays() {
        let m = Mixer::default();
        m.push(7, &[1000; 480]);
        let mut out = [0i16; 160];
        m.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0), "should wait for PRIME");
        m.push(7, &[1000; 480]);
        m.pull(&mut out);
        assert!(out.iter().all(|s| *s == 1000));
        assert_eq!(m.speaking(), vec![7]);
        // Their volume at 50% for me.
        m.set_gain(7, 0.5);
        m.pull(&mut out);
        assert!(out.iter().all(|s| *s == 500));
    }
}
