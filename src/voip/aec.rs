//! Echo cancelling: takes what the speakers played back out of the
//! microphone, so people on laptop speakers don't hear themselves.
//!
//! Two stages, run on 5 ms sub-blocks at 48 kHz:
//!
//! 1. A linear adaptive filter learns the path from the speakers to the mic
//!    (room, case, delay through the sound drivers) and subtracts its guess
//!    of the echo. It's a partitioned frequency-domain NLMS filter with
//!    overlap-save, covering 320 ms of delay and reverb, the same family
//!    of filter the well-known call apps use. While you talk over someone
//!    its steps are clipped, so your voice doesn't throw it off.
//! 2. A residual suppressor, per frequency, takes down what's left of the
//!    echo where the filter's guess says there is echo and leaves the
//!    rest alone.
//!
//! No libraries: the FFT is a plain radix-2 one written here.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Mutex;

/// Samples per sub-block (5 ms).
pub const HOP: usize = 240;
/// FFT size.
const M: usize = 512;
const BINS: usize = M / 2 + 1;
/// Filter partitions of HOP taps each: 64 × 5 ms = 320 ms of echo path.
const PARTS: usize = 64;
/// Step size.
const MU: f32 = 0.5;
/// Echo louder than this many times the reference is treated as you talking
/// (the step is clipped), per frequency.
const MAX_ECHO_GAIN: f32 = 2.0;
/// Below this reference power per bin, the filter doesn't learn.
const QUIET: f32 = 1e-7;

// -------------------------------------------------------------------- FFT

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct C {
    re: f32,
    im: f32,
}

impl C {
    fn new(re: f32, im: f32) -> C {
        C { re, im }
    }
    fn mul(self, o: C) -> C {
        C::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    /// conj(self) * o
    fn cmul(self, o: C) -> C {
        C::new(self.re * o.re + self.im * o.im, self.re * o.im - self.im * o.re)
    }
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    fn scale(self, s: f32) -> C {
        C::new(self.re * s, self.im * s)
    }
    fn norm(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

/// An in-place radix-2 FFT of size M, with the twiddles worked out once.
struct Fft {
    tw: Vec<C>,
    rev: Vec<usize>,
    buf: Vec<C>,
}

impl Fft {
    fn new() -> Fft {
        let bits = M.trailing_zeros();
        let rev = (0..M).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        let tw = (0..M / 2).map(|k| {
            let a = -2.0 * PI * k as f32 / M as f32;
            C::new(a.cos(), a.sin())
        });
        Fft { tw: tw.collect(), rev, buf: vec![C::default(); M] }
    }

    fn run(&mut self, inverse: bool) {
        let b = &mut self.buf;
        for i in 0..M {
            let j = self.rev[i];
            if i < j {
                b.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= M {
            let step = M / len;
            for start in (0..M).step_by(len) {
                for k in 0..len / 2 {
                    let mut w = self.tw[k * step];
                    if inverse {
                        w.im = -w.im;
                    }
                    let a = b[start + k];
                    let t = b[start + k + len / 2].mul(w);
                    b[start + k] = a.add(t);
                    b[start + k + len / 2] = a.sub(t);
                }
            }
            len <<= 1;
        }
    }

    /// Real signal (M samples) to its BINS-bin spectrum.
    fn forward(&mut self, x: &[f32], out: &mut [C]) {
        for (b, s) in self.buf.iter_mut().zip(x) {
            *b = C::new(*s, 0.0);
        }
        self.run(false);
        out.copy_from_slice(&self.buf[..BINS]);
    }

    /// BINS-bin spectrum back to M real samples (scaled by 1/M).
    fn inverse(&mut self, spec: &[C], out: &mut [f32]) {
        self.buf[..BINS].copy_from_slice(spec);
        for k in BINS..M {
            let c = spec[M - k];
            self.buf[k] = C::new(c.re, -c.im);
        }
        self.run(true);
        let s = 1.0 / M as f32;
        for (o, b) in out.iter_mut().zip(&self.buf) {
            *o = b.re * s;
        }
    }
}

// --------------------------------------------------------------- the filter

/// What the speakers just played, handed from the playback callback to the
/// mic callback. The two run on their own clocks, so the queue is kept
/// between a low and a high mark; going over drops the oldest (the filter
/// then re-learns the shifted delay in a moment).
pub struct Reference {
    q: Mutex<VecDeque<f32>>,
}

/// More than 150 ms waiting: trim to 40 ms.
const REF_HIGH: usize = 7200;
const REF_TARGET: usize = 1920;

impl Default for Reference {
    fn default() -> Self {
        Reference { q: Mutex::new(VecDeque::with_capacity(REF_HIGH + 4800)) }
    }
}

impl Reference {
    pub fn push(&self, played: &[f32]) {
        let mut q = self.q.lock().unwrap();
        q.extend(played.iter());
        if q.len() > REF_HIGH {
            let drop = q.len() - REF_TARGET;
            q.drain(..drop);
        }
    }

    /// The next `out.len()` played samples, or silence if nothing has been
    /// played (yet).
    pub fn pop(&self, out: &mut [f32]) {
        let mut q = self.q.lock().unwrap();
        if q.len() < out.len() {
            out.fill(0.0);
            return;
        }
        for o in out.iter_mut() {
            *o = q.pop_front().unwrap_or(0.0);
        }
    }
}

pub struct EchoCanceller {
    fft: Fft,
    /// The last M reference samples.
    x_win: Vec<f32>,
    /// Spectra of the reference window, newest first, one per partition.
    x_hist: VecDeque<Vec<C>>,
    /// Reference power per bin, summed over all partitions.
    x_pow: Vec<f32>,
    blocks: u64,
    /// The learning filter, one spectrum per partition.
    h: Vec<Vec<C>>,
    /// The filter in use.
    hf: Vec<Vec<C>>,
    /// Smoothed energies: mic, background error, foreground error.
    sd: f32,
    sb: f32,
    sf: f32,
    /// Which partition gets its gradient constrained this block.
    rotate: usize,
    // residual suppressor (sqrt-Hann, 50% overlap, frames of 2 × HOP)
    win: Vec<f32>,
    e_prev: Vec<f32>,
    y_prev: Vec<f32>,
    ola: Vec<f32>,
    see: Vec<f32>,
    syy: Vec<f32>,
    gain: Vec<f32>,
    // scratch
    spec: Vec<C>,
    espec: Vec<C>,
    time: Vec<f32>,
    /// Times the learning filter went off and was put back, for tests.
    pub resets: u32,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoCanceller {
    pub fn new() -> EchoCanceller {
        let win = (0..2 * HOP).map(|n| (PI * (n as f32 + 0.5) / (2 * HOP) as f32).sin()).collect();
        EchoCanceller {
            fft: Fft::new(),
            x_win: vec![0.0; M],
            x_hist: (0..PARTS).map(|_| vec![C::default(); BINS]).collect(),
            x_pow: vec![0.0; BINS],
            blocks: 0,
            h: vec![vec![C::default(); BINS]; PARTS],
            hf: vec![vec![C::default(); BINS]; PARTS],
            sd: 0.0,
            sb: 0.0,
            sf: 0.0,
            rotate: 0,
            win,
            e_prev: vec![0.0; HOP],
            y_prev: vec![0.0; HOP],
            ola: vec![0.0; HOP],
            see: vec![0.0; BINS],
            syy: vec![0.0; BINS],
            gain: vec![1.0; BINS],
            spec: vec![C::default(); BINS],
            espec: vec![C::default(); BINS],
            time: vec![0.0; M],
            resets: 0,
        }
    }

    /// One sub-block: `mic` holds HOP microphone samples and is replaced by
    /// the cleaned signal (5 ms behind, from the suppressor's overlap);
    /// `played` is the HOP samples the speakers played at the same moment.
    pub fn process(&mut self, mic: &mut [f32], played: &[f32]) {
        let (e, y) = self.linear(mic, played);
        self.suppress(&e, &y, mic);
    }

    /// The adaptive filter alone: returns (mic minus echo guess, echo guess).
    fn linear(&mut self, mic: &[f32], played: &[f32]) -> (Vec<f32>, Vec<f32>) {
        debug_assert_eq!(mic.len(), HOP);
        // Newest reference window and its spectrum.
        self.x_win.copy_within(HOP.., 0);
        self.x_win[M - HOP..].copy_from_slice(played);
        let mut newest = self.x_hist.pop_back().unwrap();
        for (p, x) in self.x_pow.iter_mut().zip(&newest) {
            *p = (*p - x.norm()).max(0.0);
        }
        self.fft.forward(&self.x_win, &mut newest);
        for (p, x) in self.x_pow.iter_mut().zip(&newest) {
            *p += x.norm();
        }
        self.x_hist.push_front(newest);
        // Once a second, add it up afresh so rounding doesn't build up.
        self.blocks += 1;
        if self.blocks % 200 == 0 {
            self.x_pow.fill(0.0);
            for x in &self.x_hist {
                for (p, x) in self.x_pow.iter_mut().zip(x) {
                    *p += x.norm();
                }
            }
        }

        // Two filters: the background one learns all the time; the
        // foreground one, which is what you hear, takes the background's
        // taps only once they do better. Talking over someone can knock
        // the learning off, but never what goes out.
        let yb = self.estimate(false);
        let yf = self.estimate(true);
        let e: Vec<f32> = mic.iter().zip(&yb).map(|(d, y)| d - y).collect();
        let mut ef: Vec<f32> = mic.iter().zip(&yf).map(|(d, y)| d - y).collect();
        let mut yf = yf;
        let energy = |v: &[f32]| v.iter().map(|s| s * s).sum::<f32>();
        let (pd, pb, pf) = (energy(mic), energy(&e), energy(&ef));
        self.sd = 0.9 * self.sd + 0.1 * pd;
        self.sb = 0.9 * self.sb + 0.1 * pb;
        self.sf = 0.9 * self.sf + 0.1 * pf;
        if self.sb < 0.7 * self.sf {
            self.hf.clone_from(&self.h);
            self.sf = self.sb;
        } else if self.sb > 4.0 * self.sf.max(1e-9) && self.sb > self.sd {
            // The background went off: start it again from the foreground.
            self.h.clone_from(&self.hf);
            self.sb = self.sf;
            self.resets += 1;
        }
        // Never send more than came in.
        if pf > pd {
            ef.copy_from_slice(mic);
            yf.fill(0.0);
        }

        // Learn: E = FFT[0…0, e], normalised by reference power and clipped.
        self.time[..M - HOP].fill(0.0);
        self.time[M - HOP..].copy_from_slice(&e);
        let mut espec = std::mem::take(&mut self.espec);
        self.fft.forward(&self.time, &mut espec);
        let mut any = false;
        // Quiet bins are held up to a floor, so they don't get huge steps.
        let floor = 0.01 * self.x_pow.iter().sum::<f32>() / BINS as f32 + 1e-9;
        for i in 0..BINS {
            let p = self.x_pow[i];
            if p < QUIET * PARTS as f32 {
                espec[i] = C::default();
                continue;
            }
            any = true;
            let p = p + floor;
            let mut ef = espec[i].scale(1.0 / p);
            // |E| beyond what an echo of the reference could be: clip.
            let lim = MAX_ECHO_GAIN / (p * PARTS as f32).sqrt();
            let a = ef.norm().sqrt();
            if a > lim {
                ef = ef.scale(lim / a);
            }
            espec[i] = ef.scale(MU);
        }
        if any {
            for k in 0..PARTS {
                let x = &self.x_hist[k];
                let h = &mut self.h[k];
                for i in 0..BINS {
                    h[i] = h[i].add(x[i].cmul(espec[i]));
                }
            }
            // Keep one partition (in turn) a true HOP-tap filter.
            let k = self.rotate;
            self.rotate = (self.rotate + 1) % PARTS;
            self.fft.inverse(&self.h[k], &mut self.time);
            self.time[HOP..].fill(0.0);
            let mut hk = std::mem::take(&mut self.h[k]);
            self.fft.forward(&self.time, &mut hk);
            self.h[k] = hk;
        }
        self.espec = espec;
        (ef, yf)
    }

    /// The echo guess of one filter: sum over partitions of X_k · H_k, of
    /// which the last HOP samples are valid (overlap-save).
    fn estimate(&mut self, fore: bool) -> Vec<f32> {
        let h = if fore { &self.hf } else { &self.h };
        self.spec.fill(C::default());
        for (x, h) in self.x_hist.iter().zip(h) {
            for i in 0..BINS {
                self.spec[i] = self.spec[i].add(x[i].mul(h[i]));
            }
        }
        self.fft.inverse(&self.spec, &mut self.time);
        self.time[M - HOP..].to_vec()
    }

    /// Per-frequency suppression of what echo is left, by overlap-add.
    fn suppress(&mut self, e: &[f32], y: &[f32], out: &mut [f32]) {
        // Frame = previous HOP + this HOP, windowed, zero-padded to M.
        let frame = |prev: &[f32], cur: &[f32], win: &[f32], fft: &mut Fft, spec: &mut [C], time: &mut [f32]| {
            for n in 0..HOP {
                time[n] = prev[n] * win[n];
                time[HOP + n] = cur[n] * win[HOP + n];
            }
            time[2 * HOP..].fill(0.0);
            fft.forward(time, spec);
        };
        let mut ys = vec![C::default(); BINS];
        frame(&self.y_prev, y, &self.win, &mut self.fft, &mut ys, &mut self.time);
        let mut es = vec![C::default(); BINS];
        frame(&self.e_prev, e, &self.win, &mut self.fft, &mut es, &mut self.time);
        self.e_prev.copy_from_slice(e);
        self.y_prev.copy_from_slice(y);

        for i in 0..BINS {
            self.see[i] = 0.6 * self.see[i] + 0.4 * es[i].norm();
            self.syy[i] = 0.6 * self.syy[i] + 0.4 * ys[i].norm();
            // What's left is taken to be about a fifth of the echo guess;
            // subtract twice that, keep at least -26 dB.
            let resid = 0.2 * self.syy[i];
            let g = ((self.see[i] - 2.0 * resid) / (self.see[i] + 1e-12)).clamp(0.05, 1.0);
            // Drop fast, come back a little slower.
            self.gain[i] = if g < self.gain[i] { g } else { 0.7 * self.gain[i] + 0.3 * g };
            es[i] = es[i].scale(self.gain[i]);
        }
        self.fft.inverse(&es, &mut self.time);
        for n in 0..HOP {
            out[n] = self.ola[n] + self.time[n] * self.win[n];
            self.ola[n] = self.time[HOP + n] * self.win[HOP + n];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic noise source.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }

    /// Speech-like: noise through a resonance, loudness rising and falling.
    fn talker(seed: u32, n: usize, rate: f32) -> Vec<f32> {
        let mut r = Rng(seed);
        let (mut a, mut b) = (0.0f32, 0.0f32);
        (0..n)
            .map(|i| {
                let v = r.next() + 1.6 * a - 0.8 * b;
                b = a;
                a = v;
                let env = 0.5 + 0.5 * (i as f32 * rate * 2.0 * PI / 48_000.0).sin();
                0.02 * v * env
            })
            .collect()
    }

    /// The room: 30 ms delay, then a decaying tail, gain about 0.5.
    fn room(x: &[f32]) -> Vec<f32> {
        let mut r = Rng(99);
        let delay = 1440;
        let ir: Vec<f32> = (0..2400).map(|i| r.next() * 0.04 * (-(i as f32) / 400.0).exp()).collect();
        let mut d = vec![0.0; x.len()];
        for (i, x) in x.iter().enumerate() {
            for (j, h) in ir.iter().enumerate() {
                if let Some(o) = d.get_mut(i + delay + j) {
                    *o += x * h;
                }
            }
        }
        d
    }

    fn power(s: &[f32]) -> f32 {
        s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32
    }

    #[test]
    fn fft_round_trip() {
        let mut f = Fft::new();
        let x: Vec<f32> = (0..M).map(|i| ((i * 7) % 13) as f32 - 6.0).collect();
        let mut s = vec![C::default(); BINS];
        f.forward(&x, &mut s);
        let mut back = vec![0.0; M];
        f.inverse(&s, &mut back);
        for (a, b) in x.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn learns_the_room_and_removes_the_echo() {
        let n = 48_000 * 8;
        let far = talker(1, n, 1.3);
        let echo = room(&far);
        let mut aec = EchoCanceller::new();
        let mut lin = Vec::with_capacity(n);
        let mut out = Vec::with_capacity(n);
        for (d, x) in echo.chunks(HOP).zip(far.chunks(HOP)) {
            let (e, _) = aec.linear(d, x);
            lin.extend(e);
            // The full chain, on its own canceller fed the same thing, is
            // checked below; here keep the linear part for the number.
        }
        let mut aec2 = EchoCanceller::new();
        for (d, x) in echo.chunks(HOP).zip(far.chunks(HOP)) {
            let mut m = d.to_vec();
            aec2.process(&mut m, x);
            out.extend(m);
        }
        let tail = n - 48_000 * 2;
        let erle = 10.0 * (power(&echo[tail..]) / power(&lin[tail..])).log10();
        let total = 10.0 * (power(&echo[tail..]) / power(&out[tail..])).log10();
        assert!(erle > 20.0, "filter took out only {erle:.1} dB");
        assert!(total > 30.0, "with the suppressor only {total:.1} dB");
    }

    #[test]
    fn keeps_your_voice_when_you_talk_over_them() {
        let n = 48_000 * 10;
        let far = talker(1, n, 1.3);
        let echo = room(&far);
        let start = 48_000 * 6;
        let mut near = talker(7, n, 0.9);
        near[..start].fill(0.0);
        let mic: Vec<f32> = echo.iter().zip(&near).map(|(a, b)| a + b).collect();

        let mut aec = EchoCanceller::new();
        let mut lin = Vec::with_capacity(n);
        for (d, x) in mic.chunks(HOP).zip(far.chunks(HOP)) {
            lin.extend(aec.linear(d, x).0);
        }
        // During double talk the filter output should be close to your voice:
        // echo left over and any damage well under it.
        let err: Vec<f32> = lin[start..].iter().zip(&near[start..]).map(|(a, b)| a - b).collect();
        let snr = 10.0 * (power(&near[start..]) / power(&err)).log10();
        assert!(snr > 12.0, "your voice came through only {snr:.1} dB above the leftovers");

        let mut aec2 = EchoCanceller::new();
        let mut out = Vec::with_capacity(n);
        for (d, x) in mic.chunks(HOP).zip(far.chunks(HOP)) {
            let mut m = d.to_vec();
            aec2.process(&mut m, x);
            out.extend(m);
        }
        let kept = 10.0 * (power(&out[start..]) / power(&near[start..])).log10();
        assert!(kept > -4.0, "the suppressor took {kept:.1} dB off your voice");
    }

    #[test]
    fn reference_queue_stays_bounded_and_fills_silence() {
        let r = Reference::default();
        let mut out = [1.0f32; HOP];
        r.pop(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        for _ in 0..100 {
            r.push(&[0.5; 480]);
        }
        assert!(r.q.lock().unwrap().len() <= REF_HIGH);
        r.pop(&mut out);
        assert!(out.iter().all(|s| *s == 0.5));
    }
}
