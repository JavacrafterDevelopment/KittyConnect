//! Native screen share: the app captures and encodes itself, rather than the
//! web page doing it.
//!
//! - Windows: Windows Graphics Capture for a screen or one window (covered
//!   windows keep streaming), a hardware H.264 encoder through Media
//!   Foundation (software if there's none), and WASAPI process loopback for
//!   the sound: one app's, or everything except Portside's own.
//! - macOS: ScreenCaptureKit for the picture and the sound, VideoToolbox for
//!   hardware H.264.
//!
//! Portside's own window is left out of a screen share, and its own sound
//! (the room's voices) out of the shared sound, so nobody hears themselves.
//!
//! What comes out ([`Out`]) is H.264 (Annex B, the same stream the page's
//! decoder already takes) and stereo Opus. Nothing here touches the network:
//! `voip` sends it all on the room's UDP socket, which only ever talks to
//! 127.0.0.1 — `kc-voip`, the TailCat helper with UDP, carries it from there
//! inside the encrypted tunnel.

pub mod h264;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use self::macos as platform;

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub fn sources() -> Result<Vec<Source>, String> {
        Err("Native screen sharing is only built for Windows and macOS.".into())
    }
    pub fn start(_: Options, _: Arc<Control>, _: Sink) -> Result<Box<dyn Running>, String> {
        Err("Native screen sharing is only built for Windows and macOS.".into())
    }
    pub fn audio_modes() -> Vec<&'static str> {
        Vec::new()
    }
    pub fn thumbnails(_: &[String], _: bool, _: &mut dyn FnMut(&str, Thumb)) {}
}

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use opus1::{Application, Bitrate, Channels, InbandFec, OpusEncoder, SampleRate};
use serde_json::{json, Value};

use crate::voip::audio::Resampler;

/// Something that can be shared.
#[derive(Clone, Debug)]
pub struct Source {
    /// "screen:<n>" or "window:<n>", stable while the picker is open.
    pub id: String,
    pub screen: bool,
    pub title: String,
    /// The app the window belongs to.
    pub app: String,
    pub pid: u32,
    pub w: u32,
    pub h: u32,
    pub minimized: bool,
    /// One of the windows people actually share: an app's own main window
    /// (what Alt+Tab or the Dock shows), not a background, system, overlay,
    /// tiny or untitled one. The picker shows only these unless asked for all.
    pub main: bool,
}

impl Source {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id, "kind": if self.screen { "screen" } else { "window" },
            "title": self.title, "app": self.app, "w": self.w, "h": self.h, "minimized": self.minimized,
            "main": self.screen || self.main,
        })
    }
}

/// What the platforms know about a window, for [`is_main_window`].
pub struct WindowFacts<'a> {
    pub title: &'a str,
    /// The program's file name (Windows) or the app's name (macOS).
    pub app: &'a str,
    pub w: u32,
    pub h: u32,
    /// The system itself counts it as an app window: Alt+Tab would show it
    /// (Windows), or its app is a regular Dock app (macOS).
    pub app_window: bool,
    /// Click-through or invisible: a game overlay, a notification layer.
    pub overlay: bool,
}

/// Parts of the system, and helpers that put up windows nobody shares
/// (lower case, no ".exe").
const BACKGROUND_APPS: &[&str] = &[
    // Windows shell and input.
    "textinputhost", "shellexperiencehost", "startmenuexperiencehost", "searchhost", "searchapp", "searchui",
    "lockapp", "sihost", "ctfmon", "dwm", "csrss", "winlogon", "fontdrvhost", "rundll32", "dllhost",
    "shellhost", "widgets", "widgetservice", "crossdeviceresume", "securityhealthsystray",
    // Overlays and their helpers.
    "nvidia overlay", "nvidia share", "nvcontainer", "nvsphelper64", "gamebar", "gamebarftserver",
    "gamebarpresencewriter", "xboxgamebarwidgets", "rtss", "rtsshooksloader64", "overwolf",
    "crashpad_handler", "werfault",
    // macOS.
    "dock", "window server", "windowserver", "control center", "controlcenter", "notification center",
    "notificationcenter", "spotlight", "systemuiserver", "wallpaper", "loginwindow", "screenshot",
    "screencaptureui", "textinputmenuagent", "textinputswitcher", "universalaccessauthwarn", "window manager",
    "windowmanager", "coreservicesuiagent", "usernotificationcenter",
];

/// Window titles that are the system's, whoever owns them.
const BACKGROUND_TITLES: &[&str] = &[
    "program manager", "windows input experience", "microsoft text input application", "default ime",
    "msctfime ui", "nvidia geforce overlay", "task switching", "start", "search", "notification center",
];

/// The smallest window worth listing (in pixels).
pub const MAIN_MIN_W: u32 = 160;
pub const MAIN_MIN_H: u32 = 100;

pub fn background_app(app: &str) -> bool {
    let a = app.trim().to_ascii_lowercase();
    let a = a.strip_suffix(".exe").unwrap_or(&a);
    BACKGROUND_APPS.contains(&a)
}

/// Is this a window people would want to share: a browser, a game, an editor?
/// (Like the lists other voice apps show, rather than every window there is.)
pub fn is_main_window(f: &WindowFacts) -> bool {
    let title = f.title.trim();
    f.app_window
        && !f.overlay
        && !title.is_empty()
        && f.w >= MAIN_MIN_W
        && f.h >= MAIN_MIN_H
        && !background_app(f.app)
        && !BACKGROUND_TITLES.contains(&title.to_ascii_lowercase().as_str())
}

// ------------------------------------------------------------- previews

/// A picture of what a source shows now, for the picker, and its app's icon.
#[derive(Default)]
pub struct Thumb {
    /// JPEG, at most [`THUMB_W`]×[`THUMB_H`].
    pub image: Option<Vec<u8>>,
    /// PNG, [`ICON_SIZE`] square.
    pub icon: Option<Vec<u8>>,
}

pub const THUMB_W: u32 = 400;
pub const THUMB_H: u32 = 225;
pub const ICON_SIZE: u32 = 32;

/// Pictures of these sources, one call of `each` per source as soon as it's
/// ready (slow windows don't hold the rest up). `icons`: also each app's icon.
/// Nothing here goes on the network; the pictures only go to the page.
pub fn thumbnails(ids: &[String], icons: bool, each: &mut dyn FnMut(&str, Thumb)) {
    platform::thumbnails(ids, icons, each)
}

/// A preview's size: `src` shrunk to fit in `max_w`×`max_h`, same shape,
/// never larger than the source.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub fn thumb_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (max_w.max(1), max_h.max(1));
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
    (((src_w as f64 * scale).round() as u32).max(1), ((src_h as f64 * scale).round() as u32).max(1))
}

/// BGRA rows to BGR (what JPEG takes), dropping any row padding.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn bgra_to_bgr(px: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 3);
    for row in px.chunks(stride).take(h) {
        for p in row[..w * 4].chunks_exact(4) {
            out.extend_from_slice(&p[..3]);
        }
    }
    out
}

/// Premultiplied BGRA (what Windows draws icons as) to straight BGRA (what
/// PNG keeps). False if nothing has any alpha: an old icon with a mask
/// instead, which is then drawn on a plain background.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn unpremultiply(px: &mut [u8]) -> bool {
    if px.chunks_exact(4).all(|p| p[3] == 0) {
        return false;
    }
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audio {
    Off,
    /// Only the shared window's app (and what it starts).
    App,
    /// Everything this computer plays, except Portside itself.
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Text,
    Detail,
    Motion,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub source: Source,
    /// The output's height at most (None: the source's own size).
    pub max_h: Option<u32>,
    pub fps: u32,
    /// The most the share may use; adaptive bitrate stays at or below it.
    pub bitrate: u32,
    pub audio: Audio,
    pub hint: Hint,
}

/// What a share sends back. Called from capture threads.
pub enum Out {
    /// The encoder is running; comes before the first frame.
    Started { w: u32, h: u32, fps: u32, codec: String, encoder: String, hardware: bool, audio: bool },
    Video { key: bool, ts: u64, data: Vec<u8> },
    /// One 20 ms stereo Opus packet of the shared sound.
    Audio { seq: u32, opus: Vec<u8> },
    /// The shared window was minimized (true) or is back (false).
    Paused(bool),
    /// Sound capture couldn't start; the picture goes on.
    AudioFailed(String),
    /// The share ended by itself (the window closed, the encoder failed).
    Ended(String),
}

pub type Sink = Arc<dyn Fn(Out) + Send + Sync>;

/// Shared between the app and a running share.
pub struct Control {
    pub stop: AtomicBool,
    key: AtomicBool,
    /// What adaptive bitrate asks for now.
    pub bitrate: AtomicU32,
    pub max_bitrate: u32,
    pub fps: u32,
}

impl Control {
    pub fn new(fps: u32, max_bitrate: u32) -> Control {
        Control { stop: AtomicBool::new(false), key: AtomicBool::new(true), bitrate: AtomicU32::new(max_bitrate), max_bitrate, fps }
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// True once after a key frame was asked for.
    pub fn take_key(&self) -> bool {
        self.key.swap(false, Ordering::Relaxed)
    }

    pub fn want_key(&self) {
        self.key.store(true, Ordering::Relaxed);
    }

    /// The frame rate to send at now. When the bitrate has had to come down a
    /// long way, fewer frames each get enough bits to stay sharp.
    pub fn fps_now(&self) -> u32 {
        fps_for(self.fps, self.bitrate.load(Ordering::Relaxed), self.max_bitrate)
    }
}

pub fn fps_for(fps: u32, bitrate: u32, max: u32) -> u32 {
    let share = bitrate as f32 / max.max(1) as f32;
    let f = if share < 0.25 {
        fps / 4
    } else if share < 0.5 {
        fps / 2
    } else {
        fps
    };
    f.max(5.min(fps)).max(1)
}

/// Spaces frames out to the frame rate asked for.
pub struct Cadence {
    next: Instant,
}

impl Cadence {
    pub fn new() -> Cadence {
        Cadence { next: Instant::now() }
    }

    /// True if a frame may go now, at `fps`.
    pub fn due(&mut self, now: Instant, fps: u32) -> bool {
        if now + Duration::from_micros(2000) < self.next {
            return false;
        }
        let step = Duration::from_micros(1_000_000 / fps.max(1) as u64);
        // Don't build up a debt after a stall.
        self.next = (self.next + step).max(now);
        true
    }
}

/// A share in progress. Dropping it stops capture.
pub trait Running: Send {}

pub struct Share {
    pub control: Arc<Control>,
    _running: Box<dyn Running>,
}

impl Drop for Share {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
    }
}

impl Share {
    pub fn set_bitrate(&self, bps: u32) {
        self.control.bitrate.store(bps.clamp(200_000, self.control.max_bitrate), Ordering::Relaxed);
    }

    pub fn key_frame(&self) {
        self.control.want_key();
    }
}

/// What can be shared now: screens first, then windows (not Portside's own).
pub fn sources() -> Result<Vec<Source>, String> {
    platform::sources()
}

/// Which audio choices this computer has ("app", "system").
pub fn audio_modes() -> Vec<&'static str> {
    platform::audio_modes()
}

pub fn start(opts: Options, sink: Sink) -> Result<Share, String> {
    let control = Arc::new(Control::new(opts.fps.clamp(1, 120), opts.bitrate));
    let running = platform::start(opts, control.clone(), sink)?;
    Ok(Share { control, _running: running })
}

/// The output size for a source: at most `max_h` tall, even numbers (the
/// encoders need that), never larger than the source.
pub fn out_size(src_w: u32, src_h: u32, max_h: Option<u32>) -> (u32, u32) {
    let (src_w, src_h) = (src_w.max(2), src_h.max(2));
    // H.264 level 5.1 tops out around 4096×2304; keep within it.
    let cap = max_h.unwrap_or(src_h).min(src_h).min(2160);
    let mut h = cap;
    let mut w = (src_w as u64 * h as u64 / src_h as u64) as u32;
    if w > 4096 {
        w = 4096;
        h = (src_h as u64 * w as u64 / src_w as u64) as u32;
    }
    (w.max(2) & !1, h.max(2) & !1)
}

/// Where a `src`-sized picture goes inside the `out` frame: as big as fits,
/// same shape, centred (black bars around it). (left, top, right, bottom).
/// (On macOS, ScreenCaptureKit does this itself.)
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn fit(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, out_w, out_h);
    }
    let scale = (out_w as f64 / src_w as f64).min(out_h as f64 / src_h as f64);
    let w = ((src_w as f64 * scale).round() as u32).min(out_w) & !1;
    let h = ((src_h as f64 * scale).round() as u32).min(out_h) & !1;
    let l = ((out_w - w) / 2) & !1;
    let t = ((out_h - h) / 2) & !1;
    (l, t, l + w, t + h)
}

// ------------------------------------------------------------------ audio

/// The shared sound as stereo Opus at 48 kHz, 20 ms a packet: music and
/// games, so it's coded for audio rather than speech.
pub struct AudioEncoder {
    left: Resampler,
    right: Resampler,
    l_in: Vec<f32>,
    r_in: Vec<f32>,
    l_out: Vec<f32>,
    r_out: Vec<f32>,
    frame: Vec<f32>,
    encoder: OpusEncoder,
    packet: Vec<u8>,
    seq: u32,
    sink: Sink,
}

pub const AUDIO_RATE: u32 = 48_000;
/// Samples per channel in one packet.
pub const AUDIO_FRAME: usize = 960;
pub const AUDIO_BITRATE: u32 = 128_000;

impl AudioEncoder {
    pub fn new(rate: u32, sink: Sink) -> Result<AudioEncoder, String> {
        let mut encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio).map_err(|e| format!("{e:?}"))?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(NonZeroU32::new(AUDIO_BITRATE).unwrap()));
        encoder.set_inband_fec(InbandFec::On);
        let _ = encoder.set_packet_loss_perc(5);
        Ok(AudioEncoder {

            left: Resampler::new(rate, AUDIO_RATE),
            right: Resampler::new(rate, AUDIO_RATE),
            l_in: Vec::new(),
            r_in: Vec::new(),
            l_out: Vec::new(),
            r_out: Vec::new(),
            frame: Vec::with_capacity(AUDIO_FRAME * 2),
            encoder,
            packet: vec![0u8; 1500],
            seq: 0,
            sink,
        })
    }

    /// Interleaved samples, `channels` of them per frame (mono is copied to
    /// both sides; more than two keep the first two).
    pub fn feed_interleaved(&mut self, data: &[f32], channels: usize) {
        let ch = channels.max(1);
        self.l_in.clear();
        self.r_in.clear();
        for f in data.chunks_exact(ch) {
            self.l_in.push(f[0]);
            self.r_in.push(if ch > 1 { f[1] } else { f[0] });
        }
        self.run();
    }

    /// Separate left and right planes (what macOS gives).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn feed_planar(&mut self, left: &[f32], right: &[f32]) {
        self.l_in.clear();
        self.r_in.clear();
        self.l_in.extend_from_slice(left);
        self.r_in.extend_from_slice(right);
        self.run();
    }

    fn run(&mut self) {
        let (mut l, mut r) = (Vec::new(), Vec::new());
        self.left.process(&self.l_in, &mut l);
        self.right.process(&self.r_in, &mut r);
        self.l_out.extend(l);
        self.r_out.extend(r);
        while self.l_out.len() >= AUDIO_FRAME && self.r_out.len() >= AUDIO_FRAME {
            self.frame.clear();
            for i in 0..AUDIO_FRAME {
                self.frame.push(self.l_out[i]);
                self.frame.push(self.r_out[i]);
            }
            self.l_out.drain(..AUDIO_FRAME);
            self.r_out.drain(..AUDIO_FRAME);
            // Digital silence isn't sent: the watcher's player just goes quiet.
            if self.frame.iter().all(|s| s.abs() < 1e-6) {
                continue;
            }
            if let Ok(n) = self.encoder.encode(&self.frame, AUDIO_FRAME, &mut self.packet) {
                let seq = self.seq;
                self.seq = self.seq.wrapping_add(1);
                (self.sink)(Out::Audio { seq, opus: self.packet[..n].to_vec() });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The tunnel rule: capture never touches the network. Its frames and
    /// sound only leave through `voip`'s room socket on 127.0.0.1, which
    /// kc-voip carries inside the encrypted TailCat tunnel.
    #[test]
    fn capture_has_no_network_code() {
        let sources = [
            ("mod.rs", include_str!("mod.rs")),
            ("windows.rs", include_str!("windows.rs")),
            ("macos.rs", include_str!("macos.rs")),
            ("h264.rs", include_str!("h264.rs")),
        ];
        let banned = ["std::net", "UdpSocket", "TcpStream", "TcpListener", "socket2", "WinSock", "Networking", "NSURL", "CFSocket", "nw_"];
        for (name, src) in sources {
            // Only the code, not this test's own list.
            let code = src.split("fn capture_has_no_network_code").next().unwrap();
            for b in banned {
                assert!(!code.contains(b), "{name} mentions {b}");
            }
        }
    }

    #[test]
    fn only_real_app_windows_are_main() {
        let w = |title, app, w, h| WindowFacts { title, app, w, h, app_window: true, overlay: false };
        assert!(is_main_window(&w("YouTube - Google Chrome", "chrome", 1600, 900)));
        assert!(is_main_window(&w("Minecraft 1.21", "javaw", 854, 480)));
        assert!(is_main_window(&w("Untitled - Notepad", "Notepad.exe", 800, 600)));
        assert!(!is_main_window(&w("", "chrome", 1600, 900)), "untitled");
        assert!(!is_main_window(&w("Tooltip", "chrome", 120, 40)), "tiny");
        assert!(!is_main_window(&w("Program Manager", "explorer", 1920, 1080)), "the desktop");
        assert!(!is_main_window(&w("NVIDIA GeForce Overlay", "NVIDIA Overlay", 1920, 1080)));
        assert!(!is_main_window(&w("Windows Input Experience", "TextInputHost", 1920, 1080)));
        assert!(!is_main_window(&w("Dock", "Dock", 1920, 80)));
        assert!(!is_main_window(&w("Wallpaper-", "Wallpaper", 1920, 1080)));
        assert!(!is_main_window(&WindowFacts { app_window: false, ..w("Hidden helper", "steam", 800, 600) }));
        assert!(!is_main_window(&WindowFacts { overlay: true, ..w("Steam Overlay", "gameoverlayui", 1920, 1080) }));
        assert!(background_app("SearchHost.exe"));
        assert!(!background_app("explorer"), "File Explorer windows are real");
    }

    #[test]
    fn previews_keep_the_shape_and_never_grow() {
        assert_eq!(thumb_size(1920, 1080, 400, 225), (400, 225));
        assert_eq!(thumb_size(1080, 1920, 400, 225), (127, 225));
        assert_eq!(thumb_size(3440, 1440, 400, 225), (400, 167));
        assert_eq!(thumb_size(200, 100, 400, 225), (200, 100));
        assert_eq!(thumb_size(0, 0, 400, 225), (400, 225));
    }

    #[test]
    fn pixel_helpers() {
        // Two pixels a row, padded to 12 bytes.
        let px = [1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, 7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0];
        assert_eq!(bgra_to_bgr(&px, 2, 2, 12), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let mut icon = [64, 32, 0, 128, 0, 0, 0, 0, 255, 255, 255, 255];
        assert!(unpremultiply(&mut icon));
        assert_eq!(&icon[..4], &[128, 64, 0, 128]);
        assert_eq!(&icon[8..], &[255, 255, 255, 255]);
        let mut old = [10, 20, 30, 0];
        assert!(!unpremultiply(&mut old), "no alpha at all");
    }

    #[test]
    fn output_sizes_are_even_and_capped() {
        assert_eq!(out_size(1920, 1080, None), (1920, 1080));
        assert_eq!(out_size(1920, 1080, Some(720)), (1280, 720));
        assert_eq!(out_size(1280, 720, Some(1080)), (1280, 720), "never upscaled");
        assert_eq!(out_size(1001, 777, None), (1000, 776));
        assert_eq!(out_size(5120, 2880, None), (3840, 2160));
        let (w, h) = out_size(7680, 1080, None);
        assert!(w <= 4096 && h % 2 == 0, "{w}×{h}");
    }

    #[test]
    fn fit_keeps_the_shape() {
        assert_eq!(fit(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
        // A tall window in a wide frame: bars left and right.
        let (l, t, r, b) = fit(800, 1200, 1280, 720);
        assert_eq!((t, b), (0, 720));
        assert_eq!(r - l, 480);
        assert_eq!(l, 400);
    }

    #[test]
    fn frame_rate_steps_down_with_bitrate() {
        assert_eq!(fps_for(60, 10_000_000, 10_000_000), 60);
        assert_eq!(fps_for(60, 4_000_000, 10_000_000), 30);
        assert_eq!(fps_for(60, 2_000_000, 10_000_000), 15);
        assert_eq!(fps_for(15, 100_000, 10_000_000), 5);
    }

    #[test]
    fn cadence_spaces_frames() {
        let mut c = Cadence::new();
        let t0 = Instant::now();
        assert!(c.due(t0, 30));
        assert!(!c.due(t0 + Duration::from_millis(10), 30));
        assert!(c.due(t0 + Duration::from_millis(34), 30));
    }

    #[test]
    fn audio_becomes_stereo_opus_and_silence_is_skipped() {
        let got: Arc<Mutex<Vec<(u32, usize)>>> = Arc::default();
        let g = got.clone();
        let sink: Sink = Arc::new(move |o| {
            if let Out::Audio { seq, opus } = o {
                g.lock().unwrap().push((seq, opus.len()));
            }
        });
        let mut enc = AudioEncoder::new(44_100, sink).unwrap();
        enc.feed_interleaved(&vec![0.0; 44_100 / 5 * 2], 2);
        assert!(got.lock().unwrap().is_empty(), "silence sends nothing");
        let tone: Vec<f32> = (0..44_100 / 2).flat_map(|i| {
            let s = (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.3;
            [s, -s]
        }).collect();
        enc.feed_interleaved(&tone, 2);
        let p = got.lock().unwrap();
        assert!(p.len() >= 20, "half a second is about 25 packets, got {}", p.len());
        assert!(p.windows(2).all(|w| w[1].0 == w[0].0 + 1));
        // Real packets, within one datagram (the first may be near silent).
        assert!(p.iter().all(|(_, n)| *n > 2 && *n < 1200), "{p:?}");
        assert!(p.iter().filter(|(_, n)| *n > 100).count() >= 20);
    }
}
