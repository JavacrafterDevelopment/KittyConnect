//! The sound of the screen shares you're watching: stereo Opus from each
//! sharer (see `capture::AudioEncoder`), held briefly so packets play in
//! order, each at its own volume, mixed and played on their own output stream. It plays
//! whether or not you're in voice, and goes quiet when you deafen.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use opus1::{Channels, OpusDecoder, SampleRate};

use super::audio::Resampler;

const RATE: u32 = 48_000;
/// Samples per channel in a packet (20 ms).
const FRAME: usize = 960;
/// Packets held before playing starts (60 ms), to ride out jitter.
const PRIME: usize = 3;
/// More waiting than this (a stall, then a burst): skip ahead.
const MAX_PENDING: usize = 25;
const TRIM_TO: usize = 4;
/// A jump in sequence this big means a new stream, not a loss.
const MAX_GAP: u32 = 25;
/// Packets concealed in a row before going quiet.
const MAX_PLC: u32 = 3;

/// One sharer's sound.
#[derive(Default)]
struct Inner {
    decoder: Option<OpusDecoder>,
    pending: BTreeMap<u32, Vec<u8>>,
    next: Option<u32>,
    /// Interleaved left, right.
    buf: VecDeque<f32>,
    plc: u32,
}

/// The sound of every share being watched, each with its own volume, mixed.
pub struct ShareAudio {
    streams: Mutex<HashMap<u16, Inner>>,
    /// Each sharer's volume, 0..2 (1 when not set).
    gains: Mutex<HashMap<u16, f32>>,
    deaf: AtomicBool,
}

impl Default for ShareAudio {
    fn default() -> Self {
        ShareAudio { streams: Mutex::default(), gains: Mutex::default(), deaf: AtomicBool::new(false) }
    }
}

impl ShareAudio {
    pub fn set_gain(&self, from: u16, g: f32) {
        self.gains.lock().unwrap().insert(from, g.clamp(0.0, 2.0));
    }

    pub fn set_deaf(&self, deaf: bool) {
        self.deaf.store(deaf, Ordering::Relaxed);
    }

    /// Forgets one sharer's sound (no longer watched).
    pub fn clear(&self, from: u16) {
        self.streams.lock().unwrap().remove(&from);
    }

    /// One packet of `from`'s shared sound.
    pub fn push(&self, from: u16, seq: u32, packet: &[u8]) {
        let mut all = self.streams.lock().unwrap();
        let i = all.entry(from).or_default();
        if let Some(n) = i.next {
            let ahead = seq.wrapping_sub(n) as i32;
            if ahead < 0 && ahead > -(MAX_GAP as i32) {
                return; // that moment has been played
            }
            if ahead < 0 {
                // The sharer started again: a new stream.
                i.next = None;
                i.pending.clear();
            }
        }
        i.pending.insert(seq, packet.to_vec());
        while i.pending.len() > MAX_PENDING {
            let keep: Vec<u32> = i.pending.keys().rev().take(TRIM_TO).copied().collect();
            let first = *keep.last().unwrap();
            i.pending.retain(|s, _| *s >= first);
            if i.next.is_some() {
                i.next = Some(first);
                i.buf.clear();
            }
        }
    }

    /// Fills `out` (interleaved stereo, 48 kHz) with every stream, mixed.
    pub fn pull(&self, out: &mut [f32]) {
        out.fill(0.0);
        let deaf = self.deaf.load(Ordering::Relaxed);
        let gains = self.gains.lock().unwrap().clone();
        let mut all = self.streams.lock().unwrap();
        for (from, i) in all.iter_mut() {
            if i.next.is_none() {
                if i.pending.len() < PRIME {
                    continue;
                }
                i.next = i.pending.keys().next().copied();
                i.plc = 0;
            }
            while i.buf.len() < out.len() {
                if !decode_next(i) {
                    break;
                }
            }
            let gain = if deaf { 0.0 } else { gains.get(from).copied().unwrap_or(1.0) };
            for o in out.iter_mut() {
                match i.buf.pop_front() {
                    Some(s) => *o += s * gain,
                    None => break,
                }
            }
        }
        for o in out.iter_mut() {
            *o = o.clamp(-1.0, 1.0);
        }
    }
}

fn decode_next(i: &mut Inner) -> bool {
    let Some(n) = i.next else { return false };
    if i.decoder.is_none() {
        i.decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo).ok();
    }
    let mut pcm = [0f32; FRAME * 2];
    let got = if let Some(p) = i.pending.remove(&n) {
        i.plc = 0;
        i.decoder.as_mut().and_then(|d| d.decode(&p, FRAME, &mut pcm).ok())
    } else if let Some((&later, _)) = i.pending.iter().next() {
        if later.wrapping_sub(n) > MAX_GAP {
            i.next = Some(later);
            return true;
        }
        let fec = (later == n.wrapping_add(1)).then(|| i.pending.get(&later).cloned()).flatten();
        let d = i.decoder.as_mut();
        match (d, fec) {
            (Some(d), Some(p)) => d.decode_fec(&p, FRAME, &mut pcm).ok(),
            (Some(d), None) => d.decode(&[], FRAME, &mut pcm).ok(),
            _ => None,
        }
    } else {
        if i.plc >= MAX_PLC {
            i.next = None;
            return false;
        }
        i.plc += 1;
        i.decoder.as_mut().and_then(|d| d.decode(&[], FRAME, &mut pcm).ok())
    };
    if let Some(k) = got {
        i.buf.extend(pcm[..k * 2].iter());
    }
    i.next = Some(n.wrapping_add(1));
    true
}

/// The share's sound on the default speakers. Dropping it stops playing.
pub struct Player {
    _stream: cpal::Stream,
}

impl Player {
    pub fn start(sa: Arc<ShareAudio>) -> Result<Player, String> {
        let device = cpal::default_host().default_output_device().ok_or("No speakers found.")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let out = Out {
            sa,
            channels: config.channels().max(1) as usize,
            left: Resampler::new(RATE, config.sample_rate().0),
            right: Resampler::new(RATE, config.sample_rate().0),
            chunk: vec![0.0; 480 * 2],
            ready: VecDeque::new(),
        };
        let sc: cpal::StreamConfig = config.clone().into();
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, &sc, out),
            SampleFormat::I16 => build::<i16>(&device, &sc, out),
            SampleFormat::U16 => build::<u16>(&device, &sc, out),
            SampleFormat::I32 => build::<i32>(&device, &sc, out),
            other => Err(format!("unsupported speaker format {other:?}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Player { _stream: stream })
    }
}

struct Out {
    sa: Arc<ShareAudio>,
    channels: usize,
    left: Resampler,
    right: Resampler,
    chunk: Vec<f32>,
    /// Interleaved left, right at the device's rate.
    ready: VecDeque<f32>,
}

impl Out {
    fn next(&mut self) -> (f32, f32) {
        while self.ready.len() < 2 {
            self.sa.pull(&mut self.chunk);
            let l: Vec<f32> = self.chunk.iter().step_by(2).copied().collect();
            let r: Vec<f32> = self.chunk.iter().skip(1).step_by(2).copied().collect();
            let (mut lo, mut ro) = (Vec::new(), Vec::new());
            self.left.process(&l, &mut lo);
            self.right.process(&r, &mut ro);
            for (a, b) in lo.into_iter().zip(ro) {
                self.ready.push_back(a);
                self.ready.push_back(b);
            }
        }
        (self.ready.pop_front().unwrap_or(0.0), self.ready.pop_front().unwrap_or(0.0))
    }
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut out: Out) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let ch = out.channels;
                for frame in data.chunks_mut(ch) {
                    let (l, r) = out.next();
                    if ch == 1 {
                        frame[0] = T::from_sample_((l + r) * 0.5);
                    } else {
                        for (i, s) in frame.iter_mut().enumerate() {
                            *s = T::from_sample_(match i {
                                0 => l,
                                1 => r,
                                _ => 0.0,
                            });
                        }
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
    use crate::capture::{AudioEncoder, Out as CapOut, Sink};

    fn packets(n: usize) -> Vec<(u32, Vec<u8>)> {
        let got: Arc<Mutex<Vec<(u32, Vec<u8>)>>> = Arc::default();
        let g = got.clone();
        let sink: Sink = Arc::new(move |o| {
            if let CapOut::Audio { seq, opus } = o {
                g.lock().unwrap().push((seq, opus));
            }
        });
        let mut enc = AudioEncoder::new(48_000, sink).unwrap();
        let tone: Vec<f32> = (0..FRAME * n)
            .flat_map(|i| {
                let s = (i as f32 * 300.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.4;
                [s, s * 0.5]
            })
            .collect();
        enc.feed_interleaved(&tone, 2);
        let v = got.lock().unwrap().clone();
        v
    }

    #[test]
    fn plays_in_order_after_priming() {
        let sa = ShareAudio::default();
        let ps = packets(12);
        assert!(ps.len() >= 10);
        let mut out = vec![0f32; FRAME * 2];
        sa.push(4, ps[0].0, &ps[0].1);
        sa.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0), "waits for a few packets first");
        // Out of order is fine.
        sa.push(4, ps[2].0, &ps[2].1);
        sa.push(4, ps[1].0, &ps[1].1);
        for p in &ps[3..] {
            sa.push(4, p.0, &p.1);
        }
        let mut loud = 0;
        for _ in 0..8 {
            sa.pull(&mut out);
            let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
            if rms > 0.05 {
                loud += 1;
            }
        }
        assert!(loud >= 6, "the tone comes through ({loud} of 8)");
        // Deafened: quiet.
        sa.set_deaf(true);
        for p in packets(6) {
            sa.push(4, p.0.wrapping_add(100), &p.1);
        }
        sa.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn two_shares_mix_each_at_its_own_volume() {
        let sa = ShareAudio::default();
        let ps = packets(10);
        for p in &ps {
            sa.push(1, p.0, &p.1);
            sa.push(2, p.0, &p.1);
        }
        let rms = |sa: &ShareAudio| {
            let mut out = vec![0f32; FRAME * 2];
            let mut sum = 0.0;
            for _ in 0..4 {
                sa.pull(&mut out);
                sum += out.iter().map(|s| s * s).sum::<f32>();
            }
            sum
        };
        let both = rms(&sa);
        assert!(both > 0.0);
        // The second one muted: only the first is heard.
        sa.set_gain(2, 0.0);
        sa.set_gain(1, 0.0);
        assert_eq!(rms(&sa), 0.0, "both muted");
        sa.clear(2);
        assert_eq!(sa.streams.lock().unwrap().len(), 1);
    }
}
