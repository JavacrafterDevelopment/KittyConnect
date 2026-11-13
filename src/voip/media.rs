//! Voice and screen video over UDP, inside the TailCat tunnel.
//!
//! A room's TCP connections carry names, chat, attachments and control
//! messages. Voice and screen video go as UDP datagrams instead, so a lost
//! packet costs only itself rather than holding up everything behind it (TCP
//! delivers in order, so one loss stalls the stream until it's resent). There
//! is no TCP fallback: the UDP travels through the same WireGuard tunnel as the
//! TCP (via `kc-voip`, see `tailcat.rs`), so if the room's TCP works, so does
//! this. Every socket here is on 127.0.0.1.
//!
//! Each member has one UDP socket, connected to the room's port (the host's
//! over loopback, everyone else's to the local end of their `kc-voip forward`).
//! Datagrams, at most [`MAX_DATAGRAM`] bytes (TailCat's limit):
//!
//! ```text
//! REGISTER   member → room   [0x10][id u16][key]          every 5 s; also the keepalive
//! REGISTERED room → member   [0x11]
//! VOICE      member → room   [0x20][seq u32][opus]
//!            room → member   [0x20][from u16][seq u32][opus]
//! VIDEO      member → room   [0x30][frame u32][piece u16][pieces u16][flags u8][ts u64][data]
//!            flags: bit 0 key frame; bits 1-2 layer (0-3); bits 3-7 how many
//!            frames back the frame it builds on is (0 from older versions: 1)
//!            room → watcher  [0x30][from u16][frame u32][piece u16][pieces u16][flags u8][ts u64][data]
//! NACK       watcher → room  [0x31][sharer u16][frame u32][piece u16]...   (no pieces: all of it)
//! REPORT     watcher → room  [0x32][sharer u16][loss ‰ u16][delay ms u16][shown u16][dropped u16]
//!            room → sharer   [0x32][watcher u16][loss ‰ u16][delay ms u16][shown u16][dropped u16]
//! ```
//!
//! The room only accepts datagrams from an address that registered with a
//! member's id and the key from their welcome, so nothing else on the PC can
//! inject voice or video.
//!
//! Screen video: each encoded frame is cut into pieces of [`PIECE`] bytes and
//! sent paced ([`Sender`]), never in one burst. The room forwards pieces to
//! the watchers as they arrive and keeps the last second of them. A watcher
//! puts frames back together ([`Assembler`]), asks the room again for missing
//! pieces (NACK), and if a frame still isn't whole in time, skips to the next
//! key frame and asks for one. Once a second each watcher reports its loss and
//! queueing delay.
//!
//! Frame-rate layers: the sharer's encoder (where it can) arranges frames in
//! up to three layers, so the higher ones can be left out and what's left still
//! decodes (30, 15 or 7.5 frames a second, say). Each frame says its layer and
//! which frame it builds on. The room sends each watcher only the layers their
//! connection keeps up with ([`Thinning`]), down to key frames only, so one bad
//! connection doesn't lower the picture for everyone. The sharer's bitrate
//! follows the watcher doing best ([`RateControl`]): if even they fall behind,
//! the trouble is the sharer's or the host's own connection, shared by all.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::audio::Mixer;
use super::VoipEvent;

/// TailCat's largest UDP payload (`tailcat.MaxUDPPayload`): bigger isn't
/// guaranteed to arrive.
pub const MAX_DATAGRAM: usize = 1232;
/// Screen video bytes per datagram, leaving room for the 20-byte header.
pub const PIECE: usize = 1200;
/// Most pieces in one frame (about 4 MB, a 1440p key frame with room to spare).
pub const MAX_PIECES: usize = 3500;

pub const REGISTER: u8 = 0x10;
pub const REGISTERED: u8 = 0x11;
pub const VOICE: u8 = 0x20;
pub const VIDEO: u8 = 0x30;
pub const NACK: u8 = 0x31;
pub const REPORT: u8 = 0x32;

/// Header of a VIDEO datagram from a sharer: type, frame, piece, pieces, flags, ts.
pub const VIDEO_HEAD: usize = 1 + 4 + 2 + 2 + 1 + 8;

pub fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

pub fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]))
}

pub fn register(id: u16, key: &str) -> Vec<u8> {
    let mut d = vec![REGISTER];
    d.extend_from_slice(&id.to_be_bytes());
    d.extend_from_slice(key.as_bytes());
    d
}

/// One Opus packet from the microphone.
pub fn voice(seq: u32, opus: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(5 + opus.len());
    d.push(VOICE);
    d.extend_from_slice(&seq.to_be_bytes());
    d.extend_from_slice(opus);
    d
}

/// One encoded frame of a screen share, cut into datagrams; no layers (each
/// frame builds on the one before).
#[cfg(test)]
pub fn video_pieces(frame: u32, key: bool, ts: u64, data: &[u8]) -> Vec<Vec<u8>> {
    layered_pieces(frame, key, 0, 1, ts, data)
}

/// The flags byte: key frame, layer, and how many frames back the frame it
/// builds on is (1..=31).
fn flags(key: bool, layer: u8, back: u8) -> u8 {
    key as u8 | (layer.min(3) << 1) | (back.clamp(1, 31) << 3)
}

/// (key, layer, back) from a flags byte. Older versions only set the key bit:
/// layer 0, building on the frame before.
fn unflag(f: u8) -> (bool, u8, u8) {
    (f & 1 == 1, (f >> 1) & 3, (f >> 3).max(1))
}

/// (key, layer) of a sharer's VIDEO datagram, for the room.
pub fn video_layer(d: &[u8]) -> (bool, u8) {
    let (key, layer, _) = unflag(d.get(9).copied().unwrap_or(0));
    (key, layer)
}

/// One encoded frame of a screen share, cut into datagrams. `layer` is its
/// frame-rate layer (0 for a stream without layers), `back` how many frames
/// back the frame it builds on is.
pub fn layered_pieces(frame: u32, key: bool, layer: u8, back: u8, ts: u64, data: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(PIECE).collect() };
    let count = chunks.len().min(MAX_PIECES) as u16;
    chunks
        .into_iter()
        .take(MAX_PIECES)
        .enumerate()
        .map(|(i, c)| {
            let mut d = Vec::with_capacity(VIDEO_HEAD + c.len());
            d.push(VIDEO);
            d.extend_from_slice(&frame.to_be_bytes());
            d.extend_from_slice(&(i as u16).to_be_bytes());
            d.extend_from_slice(&count.to_be_bytes());
            d.push(flags(key, layer, back));
            d.extend_from_slice(&ts.to_be_bytes());
            d.extend_from_slice(c);
            d
        })
        .collect()
}

/// (frame, piece, pieces) from a sharer's VIDEO datagram, if it's well formed.
pub fn video_head(d: &[u8]) -> Option<(u32, u16, u16)> {
    if d.len() < VIDEO_HEAD || d.len() > VIDEO_HEAD + PIECE || d[0] != VIDEO {
        return None;
    }
    let (frame, idx, count) = (u32_at(d, 1), u16_at(d, 5), u16_at(d, 7));
    (count > 0 && count as usize <= MAX_PIECES && idx < count).then_some((frame, idx, count))
}

/// A watcher's once-a-second account of the share it's watching.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Report {
    /// Share of pieces that didn't arrive the first time (0..1).
    pub loss: f32,
    /// How much longer than usual frames are taking to arrive.
    pub delay_ms: u32,
    pub shown: u16,
    pub dropped: u16,
}

impl Report {
    fn encode(&self, kind: u8, who: u16) -> Vec<u8> {
        let mut d = vec![kind];
        d.extend_from_slice(&who.to_be_bytes());
        d.extend_from_slice(&((self.loss.clamp(0.0, 1.0) * 1000.0).round() as u16).to_be_bytes());
        d.extend_from_slice(&(self.delay_ms.min(u16::MAX as u32) as u16).to_be_bytes());
        d.extend_from_slice(&self.shown.to_be_bytes());
        d.extend_from_slice(&self.dropped.to_be_bytes());
        d
    }

    /// A watcher's REPORT datagram about `sharer`'s share.
    #[cfg(test)]
    pub fn datagram(&self, sharer: u16) -> Vec<u8> {
        self.encode(REPORT, sharer)
    }

    /// (who, report) from a REPORT datagram.
    pub fn decode(d: &[u8]) -> Option<(u16, Report)> {
        if d.len() < 11 || d[0] != REPORT {
            return None;
        }
        Some((
            u16_at(d, 1),
            Report {
                loss: u16_at(d, 3).min(1000) as f32 / 1000.0,
                delay_ms: u16_at(d, 5) as u32,
                shown: u16_at(d, 7),
                dropped: u16_at(d, 9),
            },
        ))
    }
}

// ------------------------------------------------------------------ sender

enum Item {
    /// Voice, and anything else that goes out at once, ahead of video.
    Urgent(Vec<u8>),
    /// A frame's pieces, sent paced.
    Frame(Vec<Vec<u8>>),
}

/// A member's outgoing UDP. Voice goes out the moment it's handed over; screen
/// video is paced at a rate above its bitrate, so a big key frame is spread
/// over a few frame times instead of overflowing buffers in one burst. A voice
/// packet never waits behind video. Cloned freely; the thread ends with the
/// last clone.
#[derive(Clone)]
pub struct Sender {
    tx: SyncSender<Item>,
    /// Video bytes waiting to be paced out.
    queued: Arc<AtomicUsize>,
    /// The pacing rate, bits per second.
    rate: Arc<AtomicU64>,
}

/// How long queued video may take to drain before new frames are dropped
/// (the sharer is then asked for a key frame).
const MAX_BACKLOG: Duration = Duration::from_millis(300);
/// Pieces that may go out back to back.
const BURST: usize = 6 * MAX_DATAGRAM;
/// The pacing rate until the share's bitrate is known.
const DEFAULT_PACE: u64 = 10_000_000;

impl Sender {
    pub fn new(sock: UdpSocket) -> Sender {
        let (tx, rx) = sync_channel::<Item>(1024);
        let queued = Arc::new(AtomicUsize::new(0));
        let rate = Arc::new(AtomicU64::new(DEFAULT_PACE));
        let (q, r) = (queued.clone(), rate.clone());
        std::thread::spawn(move || pace(sock, rx, q, r));
        Sender { tx, queued, rate }
    }

    pub fn urgent(&self, datagram: Vec<u8>) {
        let _ = self.tx.try_send(Item::Urgent(datagram));
    }

    /// Queues a frame's pieces. A non-key frame is refused (false) while the
    /// queue is backed up: the link can't keep up, and the frame would only
    /// arrive late.
    pub fn video(&self, key: bool, pieces: Vec<Vec<u8>>) -> bool {
        let rate = self.rate.load(Ordering::Relaxed).max(1);
        let limit = (rate / 8) as usize * MAX_BACKLOG.as_millis() as usize / 1000;
        if !key && self.queued.load(Ordering::Relaxed) > limit.max(256 * 1024) {
            return false;
        }
        let bytes: usize = pieces.iter().map(Vec::len).sum();
        self.queued.fetch_add(bytes, Ordering::Relaxed);
        if self.tx.try_send(Item::Frame(pieces)).is_err() {
            self.queued.fetch_sub(bytes, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Paces video at `bps` (bits per second).
    pub fn set_rate(&self, bps: u64) {
        self.rate.store(bps.max(500_000), Ordering::Relaxed);
    }

    /// The pacing rate for a share at `bitrate`: room for key frames, which
    /// are several times the average frame.
    pub fn pace_for(bitrate: u32) -> u64 {
        (bitrate as u64 * 5 / 2).max(1_500_000)
    }
}

fn pace(sock: UdpSocket, rx: Receiver<Item>, queued: Arc<AtomicUsize>, rate: Arc<AtomicU64>) {
    let mut video: VecDeque<Vec<u8>> = VecDeque::new();
    let mut tokens = BURST as f64;
    let mut last = Instant::now();
    let take = |item: Item, video: &mut VecDeque<Vec<u8>>| match item {
        Item::Urgent(d) => {
            let _ = sock.send(&d);
        }
        Item::Frame(pieces) => video.extend(pieces),
    };
    loop {
        if video.is_empty() {
            // Nothing to pace: wait for anything to arrive.
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(item) => take(item, &mut video),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // A timed channel wait on Windows is only as fine as the 15.6 ms
            // system timer; Rust's sleep uses a high-resolution one.
            std::thread::sleep(Duration::from_millis(1));
        }
        loop {
            match rx.try_recv() {
                Ok(item) => take(item, &mut video),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let now = Instant::now();
        let bps = rate.load(Ordering::Relaxed) as f64;
        // Up to 10 ms of sending at once, at least a few pieces, so a coarse
        // wake-up doesn't cap the rate.
        let burst = (bps / 8.0 / 100.0).max(BURST as f64);
        tokens = (tokens + now.duration_since(last).as_secs_f64() * bps / 8.0).min(burst);
        last = now;
        while let Some(len) = video.front().map(Vec::len) {
            if tokens < len as f64 {
                break;
            }
            tokens -= len as f64;
            let d = video.pop_front().unwrap();
            queued.fetch_sub(d.len(), Ordering::Relaxed);
            let _ = sock.send(&d);
            // Voice that came in meanwhile goes before the next piece.
            loop {
                match rx.try_recv() {
                    Ok(item) => take(item, &mut video),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        }
    }
}

// -------------------------------------------------------------- reassembly

/// A whole frame, ready for the decoder.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub key: bool,
    pub ts: u64,
    pub data: Vec<u8>,
}

/// What a tick of the [`Assembler`] wants sent or done.
#[derive(Default, Debug)]
pub struct Tick {
    pub nacks: Vec<Vec<u8>>,
    pub frames: Vec<Frame>,
    /// Ask the sharer for a key frame.
    pub want_key: bool,
}

/// A missing piece is asked for again once a later piece has been here this long
/// (shorter is likely just reordering).
const HOLE_WAIT: Duration = Duration::from_millis(15);
/// A frame that's stopped arriving has lost its last pieces.
const TAIL_WAIT: Duration = Duration::from_millis(60);
const NACK_RETRY: Duration = Duration::from_millis(80);
const MAX_NACKS: u8 = 4;
/// A frame that's held up later ones this long is skipped, and the stream
/// restarts from a key frame. Long enough for a resend over a relay.
const GIVE_UP: Duration = Duration::from_millis(350);
const KEY_RETRY: Duration = Duration::from_secs(1);
const MAX_FRAMES: usize = 400;
/// A jump of more frames than this is a new stream, not a gap.
const MAX_GAP: u32 = 120;
/// Most pieces named in one NACK; beyond that it asks for the whole frame.
const NACK_MAX: usize = (MAX_DATAGRAM - 7) / 2;

struct Partial {
    /// 0: only known to exist (a later frame builds on it, or came first).
    count: u16,
    key: bool,
    /// Frame-rate layer (0 until its pieces come).
    layer: u8,
    /// The frame it builds on (None for a key frame).
    base: Option<u32>,
    ts: u64,
    pieces: Vec<Option<Vec<u8>>>,
    got: u16,
    highest: u16,
    first: Instant,
    last: Instant,
    /// When a piece arrived past a missing one.
    hole_at: Option<Instant>,
    /// Pieces asked for again at least once (they count as lost).
    asked: Vec<bool>,
    whole_asked: bool,
    nack_at: Option<Instant>,
    nacks: u8,
}

impl Partial {
    fn new(now: Instant) -> Partial {
        Partial {
            count: 0,
            key: false,
            layer: 0,
            base: None,
            ts: 0,
            pieces: Vec::new(),
            got: 0,
            highest: 0,
            first: now,
            last: now,
            hole_at: None,
            asked: Vec::new(),
            whole_asked: false,
            nack_at: None,
            nacks: 0,
        }
    }

    fn complete(&self) -> bool {
        self.count > 0 && self.got == self.count
    }

    fn lost(&self) -> u32 {
        if self.count == 0 {
            return 1;
        }
        if self.whole_asked {
            return self.count as u32;
        }
        (0..self.count as usize).filter(|&i| self.asked[i] || self.pieces[i].is_none()).count() as u32
    }
}

/// Queueing delay: how much longer than the best of the last ten seconds a
/// frame took to arrive (sender clock vs ours, so only the change means anything).
struct Delay {
    epoch: Instant,
    /// Per-second minimum of (arrival − capture time), µs.
    mins: VecDeque<(Instant, i64)>,
    sum: i64,
    n: i64,
}

impl Delay {
    fn sample(&mut self, now: Instant, ts: u64) {
        let d = now.duration_since(self.epoch).as_micros() as i64 - ts as i64;
        match self.mins.back_mut() {
            Some((t, m)) if now.duration_since(*t) < Duration::from_secs(1) => *m = (*m).min(d),
            _ => self.mins.push_back((now, d)),
        }
        while self.mins.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(10)) {
            self.mins.pop_front();
        }
        self.sum += d;
        self.n += 1;
    }

    /// The last second's average over the baseline, in ms; then starts a new second.
    fn take(&mut self) -> u32 {
        let Some(base) = self.mins.iter().map(|(_, m)| *m).min() else { return 0 };
        let avg = if self.n > 0 { self.sum / self.n } else { base };
        self.sum = 0;
        self.n = 0;
        ((avg - base).max(0) / 1000) as u32
    }
}

/// Which piece of which frame a VIDEO datagram carries.
struct Head {
    frame: u32,
    idx: u16,
    count: u16,
    key: bool,
    layer: u8,
    back: u8,
    ts: u64,
}

/// Puts one sharer's frames back together and hands them over in order.
pub struct Assembler {
    sharer: u16,
    frames: BTreeMap<u32, Partial>,
    /// The next frame to hand over; None while waiting for a key frame.
    next: Option<u32>,
    newest: Option<u32>,
    /// Frames handed over lately, which later frames may build on.
    shown_recently: VecDeque<u32>,
    key_asked: Option<Instant>,
    expected: u32,
    lost: u32,
    shown: u16,
    dropped: u16,
    delay: Delay,
}

impl Assembler {
    pub fn new(sharer: u16, now: Instant) -> Assembler {
        Assembler {
            sharer,
            frames: BTreeMap::new(),
            next: None,
            newest: None,
            shown_recently: VecDeque::new(),
            key_asked: None,
            expected: 0,
            lost: 0,
            shown: 0,
            dropped: 0,
            delay: Delay { epoch: now, mins: VecDeque::new(), sum: 0, n: 0 },
        }
    }

    pub fn sharer(&self) -> u16 {
        self.sharer
    }

    fn restart(&mut self) {
        self.frames.clear();
        self.next = None;
        self.newest = None;
        self.shown_recently.clear();
    }

    /// One VIDEO datagram from the room (the room's format, with `from`).
    pub fn datagram(&mut self, d: &[u8], now: Instant) -> Vec<Frame> {
        if d.len() < VIDEO_HEAD + 2 || d[0] != VIDEO || u16_at(d, 1) != self.sharer {
            return Vec::new();
        }
        let b = &d[2..]; // the sharer's own format, after [type][from]
        let (key, layer, back) = unflag(b[9]);
        let head = Head { frame: u32_at(b, 1), idx: u16_at(b, 5), count: u16_at(b, 7), key, layer, back, ts: u64_at(b, 10) };
        self.piece(head, &b[VIDEO_HEAD..], now)
    }

    fn piece(&mut self, head: Head, data: &[u8], now: Instant) -> Vec<Frame> {
        let Head { frame, idx, count, key, layer, back, ts } = head;
        if count == 0 || count as usize > MAX_PIECES || idx >= count || data.len() > PIECE {
            return Vec::new();
        }
        if let Some(n) = self.next {
            if frame < n {
                if n - frame > MAX_GAP {
                    self.restart(); // the sharer started over
                } else {
                    return Vec::new(); // already handed over or skipped
                }
            }
        }
        if self.newest.is_some_and(|w| frame > w.saturating_add(MAX_GAP)) {
            self.give_up();
            self.restart();
        }
        // While waiting for a key frame, only a key frame and what follows it are any use.
        if self.next.is_none() && !key && !self.frames.range(..frame).any(|(_, p)| p.key) {
            return Vec::new();
        }
        let base = (!key).then(|| frame.wrapping_sub(back as u32));
        if !key {
            // A frame that builds on the one before means every frame is sent
            // (no layers, or all of them): frames skipped over were lost, and
            // are only known to exist until their pieces come. With layers,
            // gaps are frames this watcher isn't sent.
            if back == 1 {
                if let Some(w) = self.newest {
                    for f in w.saturating_add(1)..frame {
                        self.frames.entry(f).or_insert_with(|| Partial::new(now));
                    }
                }
            }
            // The frame this one builds on is needed, if it hasn't come.
            if let Some(b) = base.filter(|b| self.next.is_some_and(|n| *b >= n) && !self.shown_recently.contains(b)) {
                self.frames.entry(b).or_insert_with(|| Partial::new(now));
            }
        }
        let p = self.frames.entry(frame).or_insert_with(|| Partial::new(now));
        if p.count == 0 {
            p.count = count;
            p.key = key;
            p.layer = layer;
            p.base = base;
            p.ts = ts;
            p.pieces = vec![None; count as usize];
            p.asked = vec![false; count as usize];
        }
        if p.count != count {
            return Vec::new();
        }
        let i = idx as usize;
        if p.pieces[i].is_none() {
            if p.got == 0 && !p.whole_asked {
                self.delay.sample(now, ts);
            }
            // A piece past the next one expected leaves a hole behind it.
            let expected = if p.got == 0 { 0 } else { p.highest + 1 };
            if idx > expected {
                p.hole_at.get_or_insert(now);
            }
            p.pieces[i] = Some(data.to_vec());
            p.got += 1;
            p.highest = p.highest.max(idx);
            p.last = now;
        }
        self.newest = Some(self.newest.map_or(frame, |w| w.max(frame)));
        while self.frames.len() > MAX_FRAMES {
            let oldest = *self.frames.keys().next().unwrap();
            self.finish(oldest, false);
        }
        self.deliver()
    }

    fn finish(&mut self, frame: u32, shown: bool) -> Option<Partial> {
        let p = self.frames.remove(&frame)?;
        self.expected += (p.count as u32).max(1);
        self.lost += p.lost();
        if shown {
            self.shown = self.shown.saturating_add(1);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
        Some(p)
    }

    /// Whether a whole frame can be decoded now: a key frame, or one whose base
    /// was handed over.
    fn decodable(&self, p: &Partial) -> bool {
        p.key || p.base.is_some_and(|b| self.shown_recently.contains(&b))
    }

    fn deliver(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        loop {
            let n = match self.next {
                Some(n) => n,
                None => {
                    // Waiting for a key frame: everything before the first whole one goes.
                    let Some(k) = self.frames.iter().find(|(_, p)| p.key && p.complete()).map(|(f, _)| *f) else { break };
                    let older: Vec<u32> = self.frames.range(..k).map(|(f, _)| *f).collect();
                    for f in older {
                        self.frames.remove(&f);
                    }
                    self.shown_recently.clear();
                    self.next = Some(k);
                    k
                }
            };
            let Some((&f, p)) = self.frames.range(n..).next() else { break };
            if p.complete() && self.decodable(p) {
                let p = self.finish(f, true).unwrap();
                self.shown_recently.push_back(f);
                if self.shown_recently.len() > 32 {
                    self.shown_recently.pop_front();
                }
                out.push(Frame { key: p.key, ts: p.ts, data: p.pieces.into_iter().flatten().flatten().collect() });
                self.next = Some(f + 1);
                continue;
            }
            if p.complete() {
                // Whole, but what it builds on is gone (skipped): it can't be
                // decoded. In the base layer that breaks everything after it
                // until a key frame; higher up, only its own layer suffers.
                let base_layer = p.layer == 0;
                self.finish(f, false);
                self.next = if base_layer { None } else { Some(f + 1) };
                continue;
            }
            // `f` is held up. A later whole frame that doesn't need it goes
            // ahead if it's a key frame, or if `f` is only a higher layer
            // (nothing in the base layer builds on it). A missing base-layer
            // frame is worth waiting for: losing it costs a key frame.
            let held_layer = if p.count == 0 { 0 } else { p.layer };
            let later = self
                .frames
                .range(f + 1..)
                .find(|(_, q)| q.complete() && self.decodable(q) && (q.key || held_layer > 0))
                .map(|(g, _)| *g);
            let Some(g) = later else { break };
            let skip: Vec<u32> = self.frames.range(f..g).map(|(x, _)| *x).collect();
            for x in skip {
                self.finish(x, false);
            }
            self.next = Some(g);
        }
        out
    }

    /// Stops waiting for the frame that's holding things up. Frames that build
    /// on it can't be decoded either: in the base layer that means waiting for
    /// a key frame (and asking for one); higher up, the next frame of a lower
    /// layer carries on.
    fn give_up(&mut self) {
        let Some(n) = self.next else {
            // The key frame being waited for is broken: skip to a later one.
            let Some(k) = self.frames.iter().find(|(_, p)| p.key).map(|(f, _)| *f) else { return };
            let later_key = self.frames.range(k + 1..).find(|(_, p)| p.key).map(|(f, _)| *f);
            let skip: Vec<u32> = self.frames.range(k..later_key.unwrap_or(u32::MAX)).map(|(f, _)| *f).collect();
            for f in skip {
                self.finish(f, false);
            }
            self.key_asked = None;
            return;
        };
        let Some(f) = self.frames.range(n..).next().map(|(f, _)| *f) else { return };
        self.finish(f, false);
        self.next = Some(f + 1);
        self.key_asked = None;
    }

    /// Run every few milliseconds: asks again for missing pieces, gives up on
    /// a frame that's taking too long, and says when a key frame is needed.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut t = Tick::default();
        // The frame everything waits on: the next one, or while waiting for a
        // key frame, the key frame on its way.
        let blocking = self.next.or_else(|| self.frames.iter().find(|(_, p)| p.key).map(|(f, _)| *f));
        if let Some(b) = blocking {
            let since = self.frames.range(b + 1..).next().map(|(_, p)| p.first);
            if since.is_some_and(|s| now.duration_since(s) > GIVE_UP) && !self.frames.get(&b).is_some_and(Partial::complete) {
                self.give_up();
            }
        }
        t.frames = self.deliver();
        if self.next.is_none() && !self.frames.values().any(|p| p.key) && self.key_asked.is_none_or(|a| now.duration_since(a) > KEY_RETRY) {
            self.key_asked = Some(now);
            t.want_key = true;
        }

        let newest = self.newest;
        let sharer = self.sharer;
        let relevant = self.next.or(blocking).unwrap_or(0);
        for (&f, p) in self.frames.range_mut(relevant..) {
            if p.complete() || p.nacks >= MAX_NACKS || p.nack_at.is_some_and(|a| now.duration_since(a) < NACK_RETRY) {
                continue;
            }
            let newer = newest.is_some_and(|w| w > f);
            let stalled = now.duration_since(p.last) >= TAIL_WAIT;
            let hole = p.hole_at.is_some_and(|h| now.duration_since(h) >= HOLE_WAIT);
            let whole = p.count == 0;
            if !(stalled || hole || (newer && now.duration_since(p.last) >= HOLE_WAIT) || (whole && now.duration_since(p.first) >= HOLE_WAIT)) {
                continue;
            }
            let upto = if newer || stalled { p.count } else { p.highest };
            let missing: Vec<u16> = (0..upto).filter(|&i| p.pieces[i as usize].is_none()).collect();
            if !whole && missing.is_empty() {
                continue;
            }
            let mut d = vec![NACK];
            d.extend_from_slice(&sharer.to_be_bytes());
            d.extend_from_slice(&f.to_be_bytes());
            if whole || missing.len() > NACK_MAX {
                p.whole_asked = true;
            } else {
                for i in &missing {
                    d.extend_from_slice(&i.to_be_bytes());
                    p.asked[*i as usize] = true;
                }
            }
            p.nack_at = Some(now);
            p.nacks += 1;
            p.hole_at = None;
            t.nacks.push(d);
        }
        t
    }

    /// The last second's numbers, for the sharer; then starts counting again.
    pub fn report(&mut self) -> Vec<u8> {
        let r = Report {
            loss: if self.expected == 0 { 0.0 } else { self.lost as f32 / self.expected as f32 },
            delay_ms: self.delay.take(),
            shown: self.shown,
            dropped: self.dropped,
        };
        self.expected = 0;
        self.lost = 0;
        self.shown = 0;
        self.dropped = 0;
        r.encode(REPORT, self.sharer)
    }
}

/// The sharer's side of layers: which frame each frame builds on. A frame of
/// layer `n` builds on the latest frame of a lower layer, a base-layer frame on
/// the latest base-layer frame (the usual temporal-layer pattern, L1T2/L1T3).
#[derive(Default)]
pub struct LayerChain {
    /// The latest frame sent of each layer, since the last key frame.
    last: [Option<u32>; 4],
}

impl LayerChain {
    /// How many frames back `frame`'s base is (for [`layered_pieces`]), or None
    /// if it can't be sent: its base wasn't, or is too far back to say.
    pub fn back(&self, frame: u32, key: bool, layer: u8) -> Option<u8> {
        if key {
            return Some(1);
        }
        let layer = layer.min(3) as usize;
        let base = if layer == 0 { self.last[0] } else { self.last[..layer].iter().flatten().max().copied() }?;
        let back = frame.wrapping_sub(base);
        (1..=31).contains(&back).then_some(back as u8)
    }

    /// Notes that `frame` went out.
    pub fn sent(&mut self, frame: u32, key: bool, layer: u8) {
        if key {
            self.last = [None; 4];
        }
        self.last[layer.min(3) as usize] = Some(frame);
    }

    /// `frame` (of `layer`) couldn't be sent: frames of that layer and above
    /// can't build on it, so they wait for a frame of a lower layer (or a key
    /// frame, for the base layer).
    pub fn lost(&mut self, layer: u8) {
        for l in self.last.iter_mut().skip(layer.min(3) as usize) {
            *l = None;
        }
    }
}

// ---------------------------------------------------------------- thinning

/// What the room sends one watcher of the share they're watching: all of its
/// frame-rate layers while they keep up, fewer when they fall behind, down to
/// key frames only. Each watcher has their own, so one slow connection (a
/// relay, a VPN, bad Wi-Fi) doesn't lower the picture for anyone else.
///
/// Leaving out a higher layer is safe at any frame, as nothing below builds
/// on it. Adding one back waits for a frame where nothing sent afterwards can
/// build on a frame that wasn't sent: a base-layer frame, or from key frames
/// only, a key frame.
#[derive(Debug)]
pub struct Thinning {
    /// The share's layers so far (1 without layers).
    top: u8,
    /// Layers sent: frames of a lower layer than this; 0 is key frames only.
    level: u8,
    /// A step back up, waiting for a frame where it's safe.
    raise: Option<u8>,
    good: u8,
    /// Bad reports in a row: one bad second (a hiccup) isn't enough.
    bad: u8,
    /// Reports to wait after a step down before another: what was already on
    /// its way takes a moment to drain.
    hold: u8,
    /// What was decided for each recent frame, so all its pieces go or none.
    decided: VecDeque<(u32, bool)>,
}

impl Default for Thinning {
    fn default() -> Thinning {
        Thinning { top: 1, level: 4, raise: None, good: 0, bad: 0, hold: 0, decided: VecDeque::new() }
    }
}

/// Behind by this much, a watcher steps down a layer; doing this well for
/// [`GOOD_REPORTS`] seconds, back up one. It goes by delay: a connection that
/// can't carry the stream (a relay, a VPN) queues it up. Scattered loss (Wi-Fi)
/// is made good by asking again, and fewer frames wouldn't help it; only loss
/// this heavy means the connection can't keep up at all.
const THIN_DELAY_MS: u32 = 400;
const THIN_LOSS: f32 = 0.25;
const FINE_DELAY_MS: u32 = 60;
const FINE_LOSS: f32 = 0.05;
const GOOD_REPORTS: u8 = 4;

impl Thinning {
    /// Whether to send this frame (any piece of it) to this watcher.
    pub fn pass(&mut self, frame: u32, key: bool, layer: u8) -> bool {
        if let Some(&(_, ok)) = self.decided.iter().rev().find(|(f, _)| *f == frame) {
            return ok;
        }
        self.top = self.top.max(layer + 1);
        if let Some(up) = self.raise {
            if key || (self.level > 0 && layer == 0) {
                self.level = up;
                self.raise = None;
            }
        }
        let ok = key || layer < self.level;
        self.decided.push_back((frame, ok));
        if self.decided.len() > 128 {
            self.decided.pop_front();
        }
        ok
    }

    /// Takes the watcher's once-a-second report: down a layer after two bad
    /// seconds in a row (so every two seconds while it lasts), up one after a
    /// few good seconds.
    pub fn report(&mut self, r: &Report) {
        self.hold = self.hold.saturating_sub(1);
        if r.delay_ms > THIN_DELAY_MS || r.loss > THIN_LOSS {
            self.good = 0;
            self.bad = self.bad.saturating_add(1);
            let now = self.level.min(self.top);
            if self.hold == 0 && self.bad >= 2 && now > 0 {
                self.level = now - 1;
                self.raise = None;
                self.bad = 0;
                // Skips the next report: it still shows the backlog.
                self.hold = 2;
            }
        } else if r.delay_ms < FINE_DELAY_MS && r.loss < FINE_LOSS {
            self.bad = 0;
            self.good += 1;
            if self.good >= GOOD_REPORTS && self.level < self.top && self.raise.is_none() {
                self.raise = Some(self.level + 1);
                self.good = 0;
            }
        } else {
            self.good = 0;
            self.bad = 0;
        }
    }

    /// How many of the share's layers this watcher gets (0: key frames only).
    #[cfg(test)]
    pub fn layers(&self) -> u8 {
        self.level.min(self.top)
    }
}

// ------------------------------------------------------------ rate control

/// The sharer's bitrate, following what the watchers report: down quickly when
/// pieces are lost or frames queue up, back up slowly once they're fine, never
/// above the chosen quality nor below a floor that keeps a usable picture.
/// It follows the watcher doing best: one whose own connection can't keep up
/// is sent fewer frames by the room ([`Thinning`]) without lowering the
/// picture for everyone; if even the best one falls behind, the trouble is
/// the sharer's (or host's) connection, which everyone shares.
pub struct RateControl {
    max: u32,
    floor: u32,
    rate: u32,
    sent: u32,
    reports: HashMap<u16, (Instant, Report)>,
    decided: Option<Instant>,
}

const DECIDE_EVERY: Duration = Duration::from_millis(900);
const REPORT_FRESH: Duration = Duration::from_millis(2500);

impl RateControl {
    pub fn new(max: u32) -> RateControl {
        let max = max.max(300_000);
        RateControl { max, floor: (max / 5).max(300_000), rate: max, sent: max, reports: HashMap::new(), decided: None }
    }

    #[cfg(test)]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Takes a watcher's report. Returns a new bitrate for the encoder when it
    /// has changed enough to be worth reconfiguring.
    pub fn report(&mut self, watcher: u16, r: Report, now: Instant) -> Option<u32> {
        self.reports.insert(watcher, (now, r));
        if self.decided.is_some_and(|t| now.duration_since(t) < DECIDE_EVERY) {
            return None;
        }
        self.reports.retain(|_, (t, _)| now.duration_since(*t) < REPORT_FRESH);
        let loss = self.reports.values().map(|(_, r)| r.loss).fold(1.0, f32::min);
        let delay = self.reports.values().map(|(_, r)| r.delay_ms).min().unwrap_or(0);
        self.decided = Some(now);
        let r = self.rate as f64;
        let next = if loss > 0.10 || delay > 400 {
            r * 0.65
        } else if loss > 0.03 || delay > 150 {
            r * 0.85
        } else if loss < 0.01 && delay < 60 {
            (r * 1.08).max(r + 50_000.0)
        } else {
            r
        };
        self.rate = (next as u32).clamp(self.floor, self.max);
        let change = (self.rate as f64 - self.sent as f64).abs() / self.sent as f64;
        if change > 0.03 {
            self.sent = self.rate;
            Some(self.rate)
        } else {
            None
        }
    }
}

// ------------------------------------------------------------- the member

/// What the room connection and the media thread share.
#[derive(Default)]
pub struct Link {
    /// The sharer this member is watching.
    watching: Mutex<Option<u16>>,
    stopped: AtomicBool,
}

impl Link {
    pub fn watch(&self, id: Option<u16>) {
        *self.watching.lock().unwrap() = id;
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    fn watching(&self) -> Option<u16> {
        *self.watching.lock().unwrap()
    }
}

/// Registration is retried this often until the room answers…
const REGISTER_FAST: Duration = Duration::from_millis(250);
/// …and then sent this often as the keepalive.
const REGISTER_EVERY: Duration = Duration::from_secs(5);
/// No answer to registration for this long: the media link is lost.
const LINK_LOST: Duration = Duration::from_secs(15);
/// Never answered this long after joining: it isn't going to work.
const LINK_FAILED: Duration = Duration::from_secs(20);

/// A member's UDP to and from the room, after the welcome: registers and keeps
/// registering (the keepalive), feeds voice to the mixer, reassembles the share
/// being watched, and reports on it. Ends when `link` is stopped.
pub fn run_member(
    sock: UdpSocket,
    id: u16,
    key: String,
    mixer: Arc<Mixer>,
    link: Arc<Link>,
    ask_key: impl Fn(u16) + Send + 'static,
    emit: impl Fn(VoipEvent) + Send + 'static,
) {
    std::thread::spawn(move || {
        sock.set_read_timeout(Some(Duration::from_millis(5))).ok();
        let hello = register(id, &key);
        let started = Instant::now();
        let mut answered: Option<Instant> = None;
        let mut state = "connecting";
        let mut registered_at: Option<Instant> = None;
        let mut asm: Option<Assembler> = None;
        let mut reported = Instant::now();
        let mut buf = vec![0u8; 65536];
        while !link.stopped.load(Ordering::Relaxed) {
            let now = Instant::now();
            let fresh = answered.is_some_and(|a| now.duration_since(a) < REGISTER_EVERY + Duration::from_secs(1));
            let every = if fresh { REGISTER_EVERY } else { REGISTER_FAST };
            if registered_at.is_none_or(|t| now.duration_since(t) >= every) {
                let _ = sock.send(&hello);
                registered_at = Some(now);
            }
            let next_state = match answered {
                Some(a) if now.duration_since(a) < LINK_LOST => "ok",
                Some(_) => "lost",
                None if now.duration_since(started) > LINK_FAILED => "failed",
                None => "connecting",
            };
            if next_state != state {
                state = next_state;
                emit(VoipEvent::Media(state));
            }

            // Follow what's being watched.
            let watching = link.watching();
            if asm.as_ref().map(Assembler::sharer) != watching {
                asm = watching.map(|s| Assembler::new(s, now));
            }

            // An error is just quiet, or on Windows an ICMP "unreachable"
            // from an earlier send; the socket is still fine.
            if let Ok(n) = sock.recv(&mut buf) {
                let d = &buf[..n];
                let now = Instant::now();
                match d.first() {
                    Some(&REGISTERED) => answered = Some(now),
                    Some(&VOICE) if d.len() > 7 => mixer.push_packet(u16_at(d, 1), u32_at(d, 3), &d[7..]),
                    Some(&VIDEO) => {
                        if let Some(a) = asm.as_mut() {
                            for f in a.datagram(d, now) {
                                emit(VoipEvent::Video { from: a.sharer(), key: f.key, ts: f.ts, data: f.data });
                            }
                        }
                    }
                    Some(&REPORT) => {
                        if let Some((watcher, report)) = Report::decode(d) {
                            emit(VoipEvent::VideoReport { watcher, report });
                        }
                    }
                    _ => {}
                }
            }

            if let Some(a) = asm.as_mut() {
                let now = Instant::now();
                let t = a.tick(now);
                for d in &t.nacks {
                    let _ = sock.send(d);
                }
                for f in t.frames {
                    emit(VoipEvent::Video { from: a.sharer(), key: f.key, ts: f.ts, data: f.data });
                }
                if t.want_key {
                    ask_key(a.sharer());
                }
                if now.duration_since(reported) >= Duration::from_secs(1) {
                    reported = now;
                    let _ = sock.send(&a.report());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    /// Feeds a frame's pieces (in the room's format) to `a`, except those in `skip`.
    fn feed(a: &mut Assembler, frame: u32, key: bool, data: &[u8], skip: &[usize], now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        for (i, p) in video_pieces(frame, key, frame as u64 * 33_000, data).into_iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            let mut d = vec![VIDEO];
            d.extend_from_slice(&7u16.to_be_bytes());
            d.extend_from_slice(&p[1..]);
            out.extend(a.datagram(&d, now));
        }
        out
    }

    #[test]
    fn pieces_fit_the_tunnel() {
        let ps = video_pieces(3, true, 99, &frame_bytes(5000, 1));
        assert_eq!(ps.len(), 5);
        assert!(ps.iter().all(|p| p.len() + 2 <= MAX_DATAGRAM), "room adds 2 bytes and must stay within TailCat's limit");
        assert_eq!(ps[4].len(), VIDEO_HEAD + 5000 - 4 * PIECE);
    }

    #[test]
    fn frames_come_out_whole_and_in_order() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        // Nothing before the first key frame.
        assert!(feed(&mut a, 0, false, &frame_bytes(3000, 0), &[], t0).is_empty());
        let k = frame_bytes(10_000, 1);
        let got = feed(&mut a, 1, true, &k, &[], t0);
        assert_eq!(got, vec![Frame { key: true, ts: 33_000, data: k }]);
        let d = frame_bytes(2500, 2);
        assert_eq!(feed(&mut a, 2, false, &d, &[], t0)[0].data, d);
        // A duplicate of a frame already shown is ignored.
        assert!(feed(&mut a, 2, false, &d, &[], t0).is_empty());
    }

    /// A lost piece is asked for again, and the frame comes out once it's back,
    /// before the frames that came after it.
    #[test]
    fn a_lost_piece_is_asked_for_and_order_is_kept() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        feed(&mut a, 1, true, &frame_bytes(2000, 1), &[], t0);
        let lossy = frame_bytes(6000, 2);
        assert!(feed(&mut a, 2, false, &lossy, &[2], t0).is_empty());
        assert!(feed(&mut a, 3, false, &frame_bytes(1000, 3), &[], t0).is_empty(), "held until frame 2 is whole");
        let t = a.tick(t0 + Duration::from_millis(20));
        assert_eq!(t.nacks.len(), 1);
        let n = &t.nacks[0];
        assert_eq!((n[0], u16_at(n, 1), u32_at(n, 3), u16_at(n, 7)), (NACK, 7, 2, 2));
        assert_eq!(n.len(), 9, "only the missing piece");
        assert!(a.tick(t0 + Duration::from_millis(30)).nacks.is_empty(), "not again straight away");
        // The resend arrives: frames 2 and 3 come out, in order.
        let resend = &video_pieces(2, false, 2 * 33_000, &lossy)[2];
        let mut d = vec![VIDEO, 0, 7];
        d.extend_from_slice(&resend[1..]);
        let out = a.datagram(&d, t0 + Duration::from_millis(40));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, lossy);
        let r = Report::decode(&a.report()).unwrap();
        assert_eq!(r.0, 7);
        assert_eq!(r.1.shown, 3);
        assert!(r.1.loss > 0.0, "a piece that needed resending counts as lost");
    }

    /// A frame that never arrives at all is asked for whole.
    #[test]
    fn a_missing_frame_is_asked_for_whole() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        feed(&mut a, 1, true, &frame_bytes(2000, 1), &[], t0);
        feed(&mut a, 3, false, &frame_bytes(2000, 3), &[], t0);
        let t = a.tick(t0 + Duration::from_millis(20));
        assert!(t.nacks.iter().any(|n| u32_at(n, 3) == 2 && n.len() == 7), "{:?}", t.nacks);
    }

    /// If a frame can't be completed in time, the stream skips to the next key
    /// frame and asks for one, rather than freezing.
    #[test]
    fn gives_up_and_restarts_from_a_key_frame() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        feed(&mut a, 1, true, &frame_bytes(2000, 1), &[], t0);
        feed(&mut a, 2, false, &frame_bytes(3000, 2), &[0], t0);
        feed(&mut a, 3, false, &frame_bytes(1000, 3), &[], t0 + Duration::from_millis(5));
        assert!(!a.tick(t0 + Duration::from_millis(100)).want_key);
        let t = a.tick(t0 + GIVE_UP + Duration::from_millis(50));
        assert!(t.want_key, "asks the sharer for a key frame");
        assert!(feed(&mut a, 4, false, &frame_bytes(1000, 4), &[], t0 + GIVE_UP).is_empty(), "deltas can't be shown now");
        let k = frame_bytes(4000, 5);
        let out = feed(&mut a, 5, true, &k, &[], t0 + GIVE_UP + Duration::from_millis(60));
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
        let r = Report::decode(&a.report()).unwrap().1;
        assert!(r.dropped >= 2, "{r:?}");
    }

    /// A whole key frame doesn't wait behind an older broken frame.
    #[test]
    fn a_whole_key_frame_skips_a_broken_one() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        feed(&mut a, 1, true, &frame_bytes(2000, 1), &[], t0);
        feed(&mut a, 2, false, &frame_bytes(3000, 2), &[1], t0);
        let out = feed(&mut a, 3, true, &frame_bytes(3000, 3), &[], t0);
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
    }

    #[test]
    fn rate_follows_the_best_watcher() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(4_500_000);
        let bad = Report { loss: 0.2, delay_ms: 0, shown: 30, dropped: 3 };
        let good = Report { loss: 0.0, delay_ms: 5, shown: 30, dropped: 0 };
        // One watcher struggling while another is fine: their own connection,
        // handled by thinning, not by lowering everyone's picture.
        rc.report(2, good, t0);
        assert_eq!(rc.report(3, bad, t0 + Duration::from_secs(1)), None);
        assert_eq!(rc.rate(), 4_500_000);
        // Everyone struggling: the sharer's own connection. Down.
        rc.report(2, bad, t0 + Duration::from_secs(2));
        let down = rc.report(3, bad, t0 + Duration::from_secs(3)).unwrap();
        assert!(down < 4_500_000 * 7 / 10, "{down}");
        assert_eq!(rc.report(3, bad, t0 + Duration::from_millis(3100)), None, "one decision a second");
        // A lot of queueing delay for everyone also slows it.
        let mut t = t0 + Duration::from_secs(4);
        for _ in 0..20 {
            rc.report(2, Report { delay_ms: 500, ..good }, t);
            rc.report(3, Report { delay_ms: 500, ..good }, t);
            t += Duration::from_secs(1);
        }
        assert_eq!(rc.rate(), 900_000, "never below the floor (a fifth of the chosen quality)");
        // Once they're fine it climbs back to the chosen quality, not past it.
        for _ in 0..60 {
            rc.report(2, good, t);
            rc.report(3, good, t);
            t += Duration::from_secs(1);
        }
        assert_eq!(rc.rate(), 4_500_000);
    }

    /// The L1T3 pattern: layers 0,2,1,2 repeating; each frame's base as the
    /// sharer works it out.
    fn l1t3(n: u32) -> Vec<(u32, bool, u8, u8)> {
        let mut chain = LayerChain::default();
        (0..n)
            .map(|f| {
                let key = f == 0;
                let layer = [0, 2, 1, 2][(f % 4) as usize];
                let back = chain.back(f, key, layer).unwrap();
                chain.sent(f, key, layer);
                (f, key, layer, back)
            })
            .collect()
    }

    fn feed_layered(a: &mut Assembler, f: (u32, bool, u8, u8), skip: &[usize], now: Instant) -> Vec<Frame> {
        let (frame, key, layer, back) = f;
        let mut out = Vec::new();
        for (i, p) in layered_pieces(frame, key, layer, back, frame as u64 * 33_000, &frame_bytes(2500, frame as u8)).into_iter().enumerate() {
            if !skip.contains(&i) {
                let mut d = vec![VIDEO, 0, 7];
                d.extend_from_slice(&p[1..]);
                out.extend(a.datagram(&d, now));
            }
        }
        out
    }

    #[test]
    fn the_sharer_works_out_each_frames_base() {
        let frames = l1t3(9);
        // 0 key; 1 (L2) on 0; 2 (L1) on 0; 3 (L2) on 2; 4 (L0) on 0; 5 (L2) on 4; ...
        let backs: Vec<u8> = frames.iter().map(|f| f.3).collect();
        assert_eq!(backs, vec![1, 1, 2, 1, 4, 1, 2, 1, 4]);
        // A base-layer frame that couldn't go out: nothing can build on it until a key frame.
        let mut chain = LayerChain::default();
        chain.sent(0, true, 0);
        chain.lost(0);
        assert_eq!(chain.back(4, false, 0), None);
        assert_eq!(chain.back(5, false, 2), None);
        assert_eq!(chain.back(8, true, 0), Some(1));
        // Only the top layer lost: the rest carries on.
        let mut chain = LayerChain::default();
        chain.sent(0, true, 0);
        chain.lost(2);
        assert_eq!(chain.back(2, false, 1), Some(2));
        assert_eq!(chain.back(4, false, 0), Some(4));
    }

    #[test]
    fn layers_survive_the_older_flags() {
        assert_eq!(unflag(flags(true, 0, 1)), (true, 0, 1));
        assert_eq!(unflag(flags(false, 2, 4)), (false, 2, 4));
        // What an older sharer sends: only the key bit.
        assert_eq!(unflag(0), (false, 0, 1));
        assert_eq!(unflag(1), (true, 0, 1));
    }

    /// A watcher sent only some layers shows every frame it gets, without
    /// asking for the ones it isn't sent.
    #[test]
    fn a_thinned_watcher_shows_what_it_gets() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        let mut shown = Vec::new();
        for f in l1t3(13).into_iter().filter(|f| f.2 < 2) {
            shown.extend(feed_layered(&mut a, f, &[], t0).into_iter().map(|fr| fr.ts / 33_000));
        }
        assert_eq!(shown, vec![0, 2, 4, 6, 8, 10, 12]);
        let t = a.tick(t0 + Duration::from_secs(1));
        assert!(t.nacks.is_empty() && !t.want_key, "{t:?}");
        let r = Report::decode(&a.report()).unwrap().1;
        assert_eq!((r.loss, r.dropped), (0.0, 0), "frames it isn't sent aren't losses");
    }

    /// A lost top-layer frame is skipped: the stream carries on without a key frame.
    #[test]
    fn a_lost_top_layer_frame_costs_only_itself() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        let frames = l1t3(9);
        let mut shown = Vec::new();
        for f in &frames {
            // Frame 3 (layer 2) never arrives.
            if f.0 != 3 {
                shown.extend(feed_layered(&mut a, *f, &[], t0).into_iter().map(|fr| fr.ts / 33_000));
            }
        }
        assert_eq!(shown, vec![0, 1, 2, 4, 5, 6, 7, 8]);
        assert!(!a.tick(t0 + Duration::from_secs(1)).want_key);
    }

    /// A lost base-layer frame is asked for again, and everything waits for it.
    #[test]
    fn a_lost_base_layer_frame_is_asked_for() {
        let t0 = Instant::now();
        let mut a = Assembler::new(7, t0);
        let frames = l1t3(9);
        let mut shown = Vec::new();
        for f in &frames[..4] {
            shown.extend(feed_layered(&mut a, *f, &[], t0).into_iter().map(|fr| fr.ts / 33_000));
        }
        // Frame 4 (layer 0) loses a piece; frames 5.. build on it.
        shown.extend(feed_layered(&mut a, frames[4], &[1], t0).into_iter().map(|fr| fr.ts / 33_000));
        for f in &frames[5..] {
            shown.extend(feed_layered(&mut a, *f, &[], t0).into_iter().map(|fr| fr.ts / 33_000));
        }
        assert_eq!(shown, vec![0, 1, 2, 3]);
        let t = a.tick(t0 + Duration::from_millis(30));
        assert!(t.nacks.iter().any(|n| u32_at(n, 3) == 4), "{:?}", t.nacks);
        // The resend arrives: the rest comes out in order.
        let (frame, key, layer, back) = frames[4];
        let resend = &layered_pieces(frame, key, layer, back, frame as u64 * 33_000, &frame_bytes(2500, 4))[1];
        let mut d = vec![VIDEO, 0, 7];
        d.extend_from_slice(&resend[1..]);
        let out: Vec<u64> = a.datagram(&d, t0 + Duration::from_millis(40)).into_iter().map(|fr| fr.ts / 33_000).collect();
        assert_eq!(out, vec![4, 5, 6, 7, 8]);
    }

    /// The room steps a slow watcher down a layer at a time, down to key
    /// frames only, and back up once they keep up, never mid-way through.
    #[test]
    fn thinning_follows_the_watcher() {
        let slow = Report { loss: 0.0, delay_ms: 900, shown: 10, dropped: 0 };
        let fine = Report { loss: 0.0, delay_ms: 10, shown: 30, dropped: 0 };
        let mut th = Thinning::default();
        let frames = l1t3(64);
        let sent = |th: &mut Thinning, range: std::ops::Range<usize>| -> Vec<u32> {
            frames[range].iter().filter(|f| th.pass(f.0, f.1, f.2)).map(|f| f.0).collect()
        };
        assert_eq!(sent(&mut th, 0..4), vec![0, 1, 2, 3], "everything while it keeps up");
        th.report(&slow);
        assert_eq!(th.layers(), 3, "one bad second is a hiccup");
        th.report(&fine);
        th.report(&slow);
        assert_eq!(th.layers(), 3, "and so is another, after a good one");
        th.report(&slow);
        assert_eq!(th.layers(), 2, "two in a row: down a layer");
        assert_eq!(sent(&mut th, 4..8), vec![4, 6], "the top layer goes");
        th.report(&slow);
        assert_eq!(th.layers(), 2, "waits for what's on its way to drain");
        th.report(&slow);
        assert_eq!(th.layers(), 1);
        assert_eq!(sent(&mut th, 8..12), vec![8]);
        th.report(&slow);
        th.report(&slow);
        assert_eq!(th.layers(), 0);
        assert_eq!(sent(&mut th, 12..16), Vec::<u32>::new(), "key frames only");
        // A frame decided once stays decided, for all its pieces.
        assert!(!th.pass(12, false, 0));
        for _ in 0..GOOD_REPORTS {
            th.report(&fine);
        }
        assert_eq!(sent(&mut th, 16..20), Vec::<u32>::new(), "back up only at a key frame");
        assert!(th.pass(64, true, 0));
        assert_eq!(th.layers(), 1);
        for _ in 0..GOOD_REPORTS {
            th.report(&fine);
        }
        // Up again at the next base-layer frame (68), not before.
        assert!(!th.pass(65, false, 2) && !th.pass(66, false, 1));
        assert!(th.pass(68, false, 0) && th.pass(70, false, 1) && !th.pass(69, false, 2));
        assert_eq!(th.layers(), 2);
    }

    /// Without layers there's nothing between everything and key frames only.
    #[test]
    fn thinning_a_stream_without_layers() {
        let mut th = Thinning::default();
        assert!(th.pass(0, true, 0) && th.pass(1, false, 0));
        let lossy = Report { loss: 0.3, delay_ms: 0, shown: 5, dropped: 5 };
        th.report(&lossy);
        th.report(&lossy);
        assert_eq!(th.layers(), 0);
        assert!(!th.pass(2, false, 0));
        assert!(th.pass(60, true, 0));
    }

    /// Voice goes out at once even while a big frame is being paced out.
    #[test]
    fn voice_does_not_wait_behind_video() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        let s = Sender::new(tx);
        s.set_rate(2_000_000); // 250 KB/s
        let t0 = Instant::now();
        assert!(s.video(true, video_pieces(0, true, 0, &frame_bytes(120_000, 0))));
        std::thread::sleep(Duration::from_millis(20));
        s.urgent(voice(1, &[9; 160]));
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 2048];
        let mut video_before = 0;
        loop {
            let n = rx.recv(&mut buf).unwrap();
            if buf[0] == VOICE {
                assert_eq!(n, 165);
                break;
            }
            video_before += 1;
        }
        let at = t0.elapsed();
        assert!(at < Duration::from_millis(200), "voice took {at:?}");
        assert!(video_before < 40, "{video_before} pieces went first; the frame is 100 and paced over ~0.5 s");
    }

    /// When the link can't keep up, new delta frames are refused (and the
    /// sharer makes a key frame) instead of queueing up seconds of video.
    #[test]
    fn a_backlog_refuses_delta_frames_but_not_key_frames() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        let s = Sender::new(tx);
        s.set_rate(500_000);
        assert!(s.video(true, video_pieces(0, true, 0, &frame_bytes(400_000, 0))));
        assert!(!s.video(false, video_pieces(1, false, 0, &frame_bytes(5_000, 1))), "delta refused while backed up");
        assert!(s.video(true, video_pieces(2, true, 0, &frame_bytes(5_000, 2))), "a key frame always goes");
    }
}
