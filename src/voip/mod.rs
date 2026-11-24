//! VOIP: a temporary room with a text channel and a voice channel, hosted on
//! one PC and joined through a TailCat code. Names, messages, voice and video
//! only ever travel inside the encrypted tunnel (or over loopback, for the
//! host). Rooms run on `kc-voip`, the TailCat-library helper that carries UDP
//! as well as TCP: chat and attachments use TCP, voice and screen video UDP.

pub mod audio;
pub mod client;
pub mod media;
pub mod server;
pub mod share_audio;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::code::{self, Kind};
use crate::tailcat::{self, Link, Proc, Role, TcEvent};
use crate::{Log, UserEvent};

pub enum VoipEvent {
    Welcome(Value),
    Message(Value),
    Speaking(Vec<u16>),
    Level { level: f32, talking: bool },
    Failed(String),
    /// A whole frame of someone's screen share, for the page to decode.
    Video { from: u16, key: bool, ts: u64, data: Vec<u8> },
    /// How voice and video are connected: "connecting", "ok", "lost" or "failed".
    Media(&'static str),
    /// A watcher's report on our screen share.
    VideoReport { watcher: u16, report: media::Report },
    /// What can be shared natively (or why nothing can).
    Sources(Result<Vec<crate::capture::Source>, String>),
    /// News from our native screen share (its frames go straight out).
    Share(ShareNews),
    /// One of our own frames, for the page's preview.
    Preview { key: bool, ts: u64, data: Vec<u8> },
    /// A picture of a screen or window (and its app's icon), for the picker.
    Thumb { id: String, thumb: crate::capture::Thumb },
    Upload { tid: u32, name: String, sent: u64, size: u64, error: Option<String> },
    Fetching { aid: u32, got: u64, size: u64 },
    Fetched { aid: u32, result: Result<std::path::PathBuf, String>, preview: bool },
}

/// What a native share tells the app (its frames and sound don't come this way).
pub enum ShareNews {
    Started { w: u32, h: u32, fps: u32, codec: String, encoder: String, hardware: bool, audio: bool },
    Paused(bool),
    AudioFailed(String),
    Ended(String),
}

/// Numbering and layering of our outgoing screen video. Shared by the page's
/// encoder (on the main thread) and the native one (on its own thread).
#[derive(Default)]
struct VideoOut {
    /// Numbers our screen share's frames; carries on across shares, so a
    /// watcher never mistakes a new share for an old one.
    frame_no: u32,
    /// Which frame each of our frames builds on (frame-rate layers).
    chain: media::LayerChain,
    /// Our outgoing video fell behind; when we last asked for a key frame.
    gap: Option<Instant>,
}

impl VideoOut {
    /// One encoded frame, cut into datagrams and paced out to the room. When
    /// the link is backed up the frame is dropped rather than queued (so the
    /// stream stays live). Returns true when the encoder should make a key
    /// frame: a base-layer frame couldn't go out, so what follows can't be
    /// decoded until one does.
    fn send(&mut self, m: &media::Sender, key: bool, layer: u8, ts: u64, data: &[u8]) -> bool {
        if data.len() > media::MAX_PIECES * media::PIECE {
            return key;
        }
        if let Some(asked) = self.gap {
            if !key {
                // Ask again if the key frame we asked for didn't make it either.
                let again = asked.elapsed() > Duration::from_secs(1);
                if again {
                    self.gap = Some(Instant::now());
                }
                return again;
            }
        }
        // A frame whose base never went out can't be decoded: a higher layer
        // waits for its next base; the base layer needs a key frame.
        let Some(back) = self.chain.back(self.frame_no, key, layer) else {
            return layer == 0 && self.start_gap();
        };
        // A frame's number is only used once it's queued, so watchers never
        // wait for one that was never sent.
        if m.video(key, media::layered_pieces(self.frame_no, key, layer, back, ts, data)) {
            self.chain.sent(self.frame_no, key, layer);
            self.frame_no = self.frame_no.wrapping_add(1);
            self.gap = None;
            return false;
        }
        // The link is backed up. A higher layer just loses this frame (and
        // what builds on it); the base layer needs a key frame.
        self.chain.lost(layer);
        layer == 0 && self.start_gap()
    }

    /// Starts waiting for a key frame. True the first time (ask the encoder).
    fn start_gap(&mut self) -> bool {
        let first = self.gap.is_none();
        if first {
            self.gap = Some(Instant::now());
        }
        first
    }

    /// A new share starts from a key frame; nothing builds on the old one.
    fn new_share(&mut self) {
        self.chain = media::LayerChain::default();
        self.gap = None;
    }
}

/// Our native screen share while it runs.
struct NativeShare {
    share: crate::capture::Share,
    /// What the room was told (size, codec...), once the encoder started.
    info: Option<Value>,
    label: String,
    audio: bool,
    paused: bool,
    max: u32,
    preview: Arc<AtomicBool>,
}

const CHAT_KEEP: usize = 500;
/// Images up to this size are fetched as soon as they're posted, to show inline.
const PREVIEW_MAX: u64 = 8 * 1024 * 1024;

/// Where room files live while a room is open; removed when it closes.
pub fn rooms_dir() -> std::path::PathBuf {
    crate::portable::data_dir().join("rooms")
}

/// Inline image previews for the current room, by generation.
pub fn preview_dir(gen: u64) -> std::path::PathBuf {
    crate::portable::data_dir().join("cache").join(format!("room{gen}"))
}

struct UploadState {
    tid: u32,
    name: String,
    sent: u64,
    size: u64,
    error: Option<String>,
    done: bool,
}

#[derive(Clone)]
struct DownloadState {
    st: &'static str,
    got: u64,
    size: u64,
    path: Option<std::path::PathBuf>,
    error: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Hosting,
    Joined,
}

pub struct Voip {
    gen: u64,
    mode: Mode,
    /// "off" | "starting" | "live" | "ended"
    st: &'static str,
    error: String,
    pub notice: String,
    room: String,
    my_name: String,
    me: u16,
    code: Option<String>,
    server: Option<server::RoomServer>,
    proc: Option<Proc>,
    conn: Option<client::Conn>,
    engine: Option<audio::Engine>,
    meter_stop: Option<Arc<AtomicBool>>,
    members: Vec<Value>,
    chat: VecDeque<Value>,
    speaking: Vec<u16>,
    in_voice: bool,
    muted: bool,
    deaf: bool,
    voice_error: String,
    devices: String,
    link: Link,
    ping_n: u64,
    pings: HashMap<u64, Instant>,
    log: Log,
    is_host: bool,
    /// A message from the host to you ("muted you for everyone").
    room_notice: String,
    sharing: Option<Value>,
    /// Our outgoing screen video's numbering (both encoders).
    out: Arc<Mutex<VideoOut>>,
    /// The native share, while there is one.
    native: Option<NativeShare>,
    /// What the native picker last listed, to start one of them.
    sources: Vec<crate::capture::Source>,
    sources_error: String,
    /// The picker's previews are being made (one batch at a time).
    thumbs_busy: Arc<AtomicBool>,
    share_error: String,
    share_notice: String,
    /// The sound of the share we're watching.
    player: Option<share_audio::Player>,
    share_volume: f32,
    /// Our screen share's bitrate, following what watchers report.
    rate: Option<media::RateControl>,
    /// How voice and video are connected (see [`VoipEvent::Media`]).
    media: &'static str,
    watching: Option<u16>,
    next_tid: u32,
    uploads: Vec<UploadState>,
    downloads: HashMap<u32, DownloadState>,
    previews: HashMap<u32, bool>,
    gains: HashMap<u16, f32>,
    /// Voice settings from the gear menu; kept for the whole run.
    audio: Arc<audio::Settings>,
}

impl Default for Voip {
    fn default() -> Self {
        Voip {
            gen: 0,
            mode: Mode::Off,
            st: "off",
            error: String::new(),
            notice: String::new(),
            room: String::new(),
            my_name: String::new(),
            me: 0,
            code: None,
            server: None,
            proc: None,
            conn: None,
            engine: None,
            meter_stop: None,
            members: Vec::new(),
            chat: VecDeque::new(),
            speaking: Vec::new(),
            in_voice: false,
            muted: false,
            deaf: false,
            voice_error: String::new(),
            devices: String::new(),
            link: Link::default(),
            ping_n: 0,
            pings: HashMap::new(),
            log: Log::default(),
            is_host: false,
            room_notice: String::new(),
            sharing: None,
            out: Arc::default(),
            native: None,
            sources: Vec::new(),
            sources_error: String::new(),
            thumbs_busy: Arc::default(),
            share_error: String::new(),
            share_notice: String::new(),
            player: None,
            share_volume: 1.0,
            rate: None,
            media: "off",
            watching: None,
            next_tid: 1,
            uploads: Vec::new(),
            downloads: HashMap::new(),
            previews: HashMap::new(),
            gains: HashMap::new(),
            audio: Arc::default(),
        }
    }
}

fn clean_name(name: &str) -> Result<String, String> {
    let n = server::clean(name, 32);
    if n.is_empty() {
        Err("Pick your name first (top right).".into())
    } else {
        Ok(n)
    }
}

impl Voip {
    pub fn is_live(&self) -> bool {
        self.st == "live"
    }

    fn reset(&mut self) {
        self.stop_voice();
        self.conn = None;
        self.proc = None;
        self.server = None;
        self.mode = Mode::Off;
        self.code = None;
        self.members.clear();
        self.speaking.clear();
        self.pings.clear();
        self.link = Link::default();
        let _ = std::fs::remove_dir_all(preview_dir(self.gen));
        self.is_host = false;
        self.room_notice.clear();
        self.sharing = None;
        self.native = None;
        self.player = None;
        self.share_error.clear();
        self.share_notice.clear();
        self.out.lock().unwrap().new_share();
        self.rate = None;
        self.media = "off";
        self.watching = None;
        self.uploads.clear();
        self.downloads.clear();
        self.previews.clear();
        self.gains.clear();
        self.gen += 1;
    }

    /// Starts a room on this PC and joins it.
    pub fn host(&mut self, room: &str, name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.reset();
        self.error.clear();
        self.notice.clear();
        self.chat.clear();
        self.log = Log::default();
        let name = match clean_name(name) {
            Ok(n) => n,
            Err(e) => return self.error = e,
        };
        let srv = match server::RoomServer::start(room, rooms_dir()) {
            Ok(s) => s,
            Err(e) => return self.error = e,
        };
        let args = vec!["serve".into(), "--key=new".into(), srv.port.to_string()];
        match tailcat::spawn(Role::VoipHost, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        self.conn = Some(client::connect(srv.port, name.clone(), Some(srv.host_token.clone()), self.audio.clone(), self.gen, proxy.clone()));
        self.server = Some(srv);
        self.my_name = name;
        self.mode = Mode::Hosting;
        self.st = "starting";
        self.media = "connecting";
    }

    /// Joins someone else's room through TailCat.
    pub fn join(&mut self, input: &str, name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.reset();
        self.error.clear();
        self.notice.clear();
        self.chat.clear();
        self.log = Log::default();
        let name = match clean_name(name) {
            Ok(n) => n,
            Err(e) => return self.error = e,
        };
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::Voip => c,
            Ok(c) if c.kind == Kind::File => return self.error = "That's a file share code. Paste it in the File Share tab.".into(),
            Ok(c) if c.kind == Kind::Mount => return self.error = "That's a mount code. Paste it in the Mounts tab.".into(),
            Ok(c) if c.kind == Kind::Hub => return self.error = "That's a hub code. Paste it in the Hub tab.".into(),
            Ok(_) => return self.error = "That's a port code. Ask for the room code (it starts with kitty:voip:).".into(),
            Err(e) => return self.error = e,
        };
        // The local end of the tunnel takes TCP and UDP on the same port: find
        // one free for both, then let it go for kc-voip to listen on.
        let local = match server::bind_pair().and_then(|(t, _udp)| t.local_addr().map(|a| a.port()).map_err(|e| e.to_string())) {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        match tailcat::spawn(Role::VoipJoin, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        self.conn = Some(client::connect(local, name.clone(), None, self.audio.clone(), self.gen, proxy.clone()));
        self.code = Some(code::voip(c.ports[0], &c.addr));
        self.my_name = name;
        self.mode = Mode::Joined;
        self.st = "starting";
        self.media = "connecting";
    }

    /// The room's code and name while we're in it, to invite people from the Hub.
    pub fn invite_code(&self) -> Option<(String, String)> {
        let code = self.code.clone().filter(|_| self.is_live())?;
        let title = if self.room.is_empty() { "Voice room".to_string() } else { self.room.clone() };
        Some((code, title))
    }

    pub fn is_on(&self) -> bool {
        self.mode_name() != "off"
    }

    pub fn is_hosting(&self) -> bool {
        matches!(self.mode, Mode::Hosting)
    }

    fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Off => "off",
            Mode::Hosting => "host",
            Mode::Joined => "join",
        }
    }

    /// For the Connections list: the room, everyone in it, and how we reach it.
    pub fn connection(&self) -> Option<Value> {
        if !self.is_on() {
            return None;
        }
        Some(json!({
            "st": self.st,
            "mode": self.mode_name(),
            "room": self.room,
            "me": self.me,
            "members": self.members,
            "link": self.link.json(),
        }))
    }

    pub fn leave(&mut self) {
        self.reset();
        self.st = "off";
        self.chat.clear();
    }

    fn end(&mut self, why: String) {
        self.reset();
        self.st = "ended";
        self.notice = why;
    }

    pub fn chat(&mut self, text: &str) {
        if let Some(c) = &self.conn {
            let text = text.trim();
            if !text.is_empty() {
                c.send(json!({ "t": "chat", "text": text }));
            }
        }
    }

    pub fn voice(&mut self, on: bool, proxy: &EventLoopProxy<UserEvent>) {
        let Some(conn) = &self.conn else { return };
        if !self.is_live() {
            return;
        }
        if !on {
            self.stop_voice();
            return;
        }
        if self.engine.is_some() {
            return;
        }
        self.voice_error.clear();
        let Some(out) = conn.media.clone() else {
            self.voice_error = "Voice couldn't open its connection.".into();
            return;
        };
        match audio::Engine::start(conn.mixer.clone(), out, self.muted) {
            Ok(engine) => {
                conn.mixer.set_deaf(self.deaf);
                self.devices = format!("{} · {}", engine.mic_name, engine.speaker_name);
                if engine.mic_name.starts_with("No microphone") || engine.mic_name.starts_with("Microphone unavailable") {
                    self.voice_error = format!("{}. You can still listen.", engine.mic_name);
                }
                // The level meter and the "you're talking" light.
                let stop = Arc::new(AtomicBool::new(false));
                let (s, lvl, talk, gen, p) = (stop.clone(), engine.level.clone(), engine.talking.clone(), self.gen, proxy.clone());
                std::thread::spawn(move || {
                    while !s.load(Ordering::Relaxed) {
                        let level = f32::from_bits(lvl.load(Ordering::Relaxed));
                        let talking = talk.load(Ordering::Relaxed);
                        let _ = p.send_event(UserEvent::Voip(gen, VoipEvent::Level { level, talking }));
                        std::thread::sleep(Duration::from_millis(120));
                    }
                });
                self.meter_stop = Some(stop);
                self.engine = Some(engine);
                self.in_voice = true;
                conn.send(json!({ "t": "voice", "on": true }));
                conn.send(json!({ "t": "mute", "muted": self.muted || self.deaf, "deaf": self.deaf }));
            }
            Err(e) => self.voice_error = e,
        }
    }

    fn stop_voice(&mut self) {
        if let Some(s) = self.meter_stop.take() {
            s.store(true, Ordering::Relaxed);
        }
        self.engine = None;
        if let Some(c) = &self.conn {
            c.mixer.clear();
            if self.in_voice {
                c.send(json!({ "t": "voice", "on": false }));
            }
        }
        self.in_voice = false;
    }

    pub fn set_mute(&mut self, muted: bool, deaf: bool) {
        self.muted = muted || deaf;
        self.deaf = deaf;
        if let Some(e) = &self.engine {
            e.muted.store(self.muted, Ordering::Relaxed);
        }
        if let Some(c) = &self.conn {
            c.mixer.set_deaf(deaf);
            c.shared_sound.set_deaf(deaf);
            c.send(json!({ "t": "mute", "muted": self.muted, "deaf": deaf }));
        }
    }

    // ------------------------------------------------------- screen share

    /// The page has started or stopped capturing; tell the room.
    pub fn screen(&mut self, info: Option<Value>) {
        let Some(c) = &self.conn else { return };
        match info {
            Some(i) => {
                c.send(json!({
                    "t": "screen", "on": true, "w": i["w"], "h": i["h"], "fps": i["fps"], "codec": i["codec"],
                    "audio": i["audio"], "paused": i["paused"], "label": i["label"],
                }));
                // The chosen quality's bitrate is the most it will ever use.
                let max = i["bitrate"].as_u64().unwrap_or(4_500_000).clamp(300_000, 50_000_000) as u32;
                if let Some(m) = &c.media {
                    m.set_rate(media::Sender::pace_for(max));
                }
                self.rate = Some(media::RateControl::new(max));
                // A new share starts from a key frame; nothing builds on the old one.
                self.out.lock().unwrap().new_share();
                self.sharing = Some(i);
            }
            None => {
                c.send(json!({ "t": "screen", "on": false }));
                self.sharing = None;
                self.rate = None;
            }
        }
    }

    /// One encoded frame from the page's encoder. `layer` is its frame-rate
    /// layer (0 without layers). Returns true when the encoder should make a
    /// key frame (see [`VideoOut::send`]).
    pub fn video(&mut self, key: bool, layer: u8, ts: u64, data: &[u8]) -> bool {
        if self.sharing.is_none() || self.native.is_some() {
            return false;
        }
        let Some(m) = self.conn.as_ref().and_then(|c| c.media.clone()) else { return false };
        self.out.lock().unwrap().send(&m, key, layer, ts, data)
    }

    // ------------------------------------------------- native screen share

    /// Lists what can be shared, off the main thread (macOS may ask for
    /// permission first). The answer comes back as [`VoipEvent::Sources`].
    pub fn list_sources(&self, proxy: &EventLoopProxy<UserEvent>) {
        let (p, gen) = (proxy.clone(), self.gen);
        std::thread::spawn(move || {
            let r = crate::capture::sources();
            let _ = p.send_event(UserEvent::Voip(gen, VoipEvent::Sources(r)));
        });
    }

    /// Pictures of what the picker lists, made off the main thread; each comes
    /// back as [`VoipEvent::Thumb`]. Asked again every few seconds while the
    /// picker is open (so the pictures are live); a batch still running makes
    /// the next ask wait for the one after. They only go to the page.
    pub fn thumbnails(&self, ids: &[String], icons: bool, proxy: &EventLoopProxy<UserEvent>) {
        let ids: Vec<String> = ids.iter().filter(|id| self.sources.iter().any(|s| &s.id == *id)).take(64).cloned().collect();
        if ids.is_empty() || self.thumbs_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let (p, gen, busy) = (proxy.clone(), self.gen, self.thumbs_busy.clone());
        std::thread::spawn(move || {
            crate::capture::thumbnails(&ids, icons, &mut |id, thumb| {
                let _ = p.send_event(UserEvent::Voip(gen, VoipEvent::Thumb { id: id.to_string(), thumb }));
            });
            busy.store(false, Ordering::Release);
        });
    }

    /// Starts sharing natively, from the picker's choices.
    pub fn share(&mut self, msg: &Value, proxy: &EventLoopProxy<UserEvent>) {
        use crate::capture::{self, Audio, Hint, Out};
        self.share_error.clear();
        self.share_notice.clear();
        if self.native.is_some() || self.sharing.is_some() || !self.is_live() {
            return;
        }
        let Some(m) = self.conn.as_ref().and_then(|c| c.media.clone()) else {
            return self.share_error = "Screen share couldn't open its connection.".into();
        };
        let id = msg["source"].as_str().unwrap_or("");
        let Some(source) = self.sources.iter().find(|s| s.id == id).cloned() else {
            return self.share_error = "That screen or window isn't there any more. Pick again.".into();
        };
        let fps = msg["fps"].as_u64().unwrap_or(30).clamp(5, 120) as u32;
        let max_h = msg["height"].as_u64().map(|h| h as u32);
        let bitrate = msg["bitrate"].as_u64().unwrap_or(6_000_000).clamp(300_000, 50_000_000) as u32;
        let audio = match msg["audio"].as_str() {
            Some("app") if !source.screen => Audio::App,
            Some("system") => Audio::System,
            _ => Audio::Off,
        };
        let hint = match msg["hint"].as_str() {
            Some("text") => Hint::Text,
            Some("motion") => Hint::Motion,
            _ => Hint::Detail,
        };
        let label = if source.screen { String::new() } else if source.app.is_empty() { source.title.clone() } else { source.app.clone() };
        let preview = Arc::new(AtomicBool::new(msg["preview"] == true));
        let opts = capture::Options { source, max_h, fps, bitrate, audio, hint };

        // Frames and sound go straight from the capture thread onto the
        // room's UDP socket (127.0.0.1, into the TailCat tunnel); only news
        // comes through the app.
        let (p, gen, out, pv) = (proxy.clone(), self.gen, self.out.clone(), preview.clone());
        let control: Arc<Mutex<Option<Arc<capture::Control>>>> = Arc::default();
        let ctl = control.clone();
        let news = move |n: ShareNews| {
            let _ = p.send_event(UserEvent::Voip(gen, VoipEvent::Share(n)));
        };
        let p2 = proxy.clone();
        let sink: capture::Sink = Arc::new(move |o| match o {
            Out::Video { key, ts, data } => {
                let again = out.lock().unwrap().send(&m, key, 0, ts, &data);
                if again {
                    if let Some(c) = ctl.lock().unwrap().as_ref() {
                        c.want_key();
                    }
                }
                if pv.load(Ordering::Relaxed) {
                    let _ = p2.send_event(UserEvent::Voip(gen, VoipEvent::Preview { key, ts, data }));
                }
            }
            Out::Audio { seq, opus } => m.urgent(media::share_audio(seq, &opus)),
            Out::Started { w, h, fps, codec, encoder, hardware, audio } => news(ShareNews::Started { w, h, fps, codec, encoder, hardware, audio }),
            Out::Paused(on) => news(ShareNews::Paused(on)),
            Out::AudioFailed(why) => news(ShareNews::AudioFailed(why)),
            Out::Ended(why) => news(ShareNews::Ended(why)),
        });
        match capture::start(opts, sink) {
            Ok(share) => {
                *control.lock().unwrap() = Some(share.control.clone());
                self.out.lock().unwrap().new_share();
                self.native = Some(NativeShare { share, info: None, label, audio: false, paused: false, max: bitrate, preview });
            }
            Err(e) => self.share_error = e,
        }
    }

    /// Stops our screen share, native or the page's.
    pub fn unshare(&mut self) {
        let was_native = self.native.take().is_some();
        if was_native {
            self.screen(None);
        }
    }

    pub fn set_preview(&self, on: bool) {
        if let Some(n) = &self.native {
            n.preview.store(on, Ordering::Relaxed);
            if on {
                n.share.key_frame();
            }
        }
    }

    pub fn set_share_volume(&mut self, v: f32) {
        self.share_volume = v.clamp(0.0, 2.0);
        if let Some(c) = &self.conn {
            c.shared_sound.set_gain(self.share_volume);
        }
    }

    /// Tells the room what we're sharing now.
    fn announce_native(&mut self) {
        let Some(n) = &self.native else { return };
        let Some(mut info) = n.info.clone() else { return };
        info["audio"] = json!(n.audio);
        info["paused"] = json!(n.paused);
        info["label"] = json!(n.label);
        if let Some(c) = &self.conn {
            c.send(json!({
                "t": "screen", "on": true, "w": info["w"], "h": info["h"], "fps": info["fps"], "codec": info["codec"],
                "audio": info["audio"], "paused": info["paused"], "label": info["label"],
            }));
        }
        self.sharing = Some(info);
    }

    fn on_share(&mut self, n: ShareNews) {
        match n {
            ShareNews::Started { w, h, fps, codec, encoder, hardware, audio } => {
                let Some(ns) = self.native.as_mut() else { return };
                let max = ns.max;
                ns.audio = audio;
                ns.info = Some(json!({
                    "w": w, "h": h, "fps": fps, "codec": codec, "bitrate": max, "native": true,
                    "encoder": encoder, "hardware": hardware,
                }));
                if let Some(m) = self.conn.as_ref().and_then(|c| c.media.as_ref()) {
                    m.set_rate(media::Sender::pace_for(max));
                }
                self.rate = Some(media::RateControl::new(max));
                self.announce_native();
            }
            ShareNews::Paused(on) => {
                if let Some(ns) = self.native.as_mut() {
                    ns.paused = on;
                }
                self.announce_native();
            }
            ShareNews::AudioFailed(why) => {
                if let Some(ns) = self.native.as_mut() {
                    ns.audio = false;
                }
                self.share_notice = format!("Sharing without sound: {why}");
                self.announce_native();
            }
            ShareNews::Ended(why) => {
                if self.native.is_some() {
                    self.unshare();
                    self.share_error = why;
                }
            }
        }
    }

    pub fn watch(&mut self, id: Option<u16>) {
        self.watching = id;
        if let Some(c) = &self.conn {
            c.link.watch(id);
            c.send(json!({ "t": "watch", "id": id }));
            c.shared_sound.clear();
            c.shared_sound.set_gain(self.share_volume);
            c.shared_sound.set_deaf(self.deaf);
        }
        self.follow_share_sound();
    }

    /// Plays the watched share's sound while it has some.
    fn follow_share_sound(&mut self) {
        let has_sound = self.watching.is_some_and(|w| self.members.iter().any(|m| m["id"] == w && m["screen"]["audio"] == true));
        if !has_sound {
            self.player = None;
            return;
        }
        if self.player.is_some() {
            return;
        }
        if let Some(c) = &self.conn {
            match share_audio::Player::start(c.shared_sound.clone()) {
                Ok(p) => self.player = Some(p),
                Err(e) => self.share_notice = format!("Couldn't play the stream's sound: {e}"),
            }
        }
    }

    pub fn request_keyframe(&self, id: u16) {
        if id == self.me {
            // Our own preview: a key frame to start from.
            if let Some(n) = &self.native {
                n.share.key_frame();
            }
            return;
        }
        if let Some(c) = &self.conn {
            c.send(json!({ "t": "keyframe", "id": id }));
        }
    }

    // -------------------------------------------------------- attachments

    pub fn attach(&mut self, paths: Vec<std::path::PathBuf>, text: String, proxy: &EventLoopProxy<UserEvent>) {
        let Some(c) = &self.conn else { return };
        for (i, p) in paths.into_iter().enumerate() {
            let tid = self.next_tid;
            self.next_tid += 1;
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            self.uploads.push(UploadState { tid, name, sent: 0, size: 0, error: None, done: false });
            // The typed message goes with the first file.
            c.upload(tid, p, if i == 0 { text.clone() } else { String::new() }, self.gen, proxy.clone());
        }
    }

    pub fn download(&mut self, aid: u32, dir: std::path::PathBuf) {
        let Some(c) = &self.conn else { return };
        if self.downloads.get(&aid).is_some_and(|d| d.st == "downloading") {
            return;
        }
        self.downloads.insert(aid, DownloadState { st: "downloading", got: 0, size: 0, path: None, error: String::new() });
        c.fetch(aid, client::Fetch { dir, preview: false });
    }

    pub fn download_path(&self, aid: u32) -> Option<std::path::PathBuf> {
        self.downloads.get(&aid).and_then(|d| d.path.clone())
    }

    fn maybe_preview(&mut self, msg: &Value) {
        let f = &msg["file"];
        if f["image"] != true || f["size"].as_u64().unwrap_or(u64::MAX) > PREVIEW_MAX {
            return;
        }
        let aid = f["aid"].as_u64().unwrap_or(0) as u32;
        if let (Some(c), false) = (&self.conn, self.previews.contains_key(&aid)) {
            self.previews.insert(aid, false);
            c.fetch(aid, client::Fetch { dir: preview_dir(self.gen), preview: true });
        }
    }

    /// From the gear menu: input volume, automatic or manual sensitivity,
    /// noise suppression, output volume. Applies at once, in or out of voice.
    pub fn set_audio(&self, v: &Value) {
        let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d) as f32;
        self.audio.set(
            f("inputGain", 1.0),
            v["autoSensitivity"].as_bool().unwrap_or(true),
            f("thresholdDb", -50.0),
            v["noiseSuppression"].as_bool().unwrap_or(true),
            f("outputGain", 1.0),
        );
    }

    // ------------------------------------------------- volume and the host

    /// Someone's volume for me only (0 = muted for me, 1 = normal, 2 = 200%).
    pub fn set_volume(&mut self, id: u16, gain: f32) {
        self.gains.insert(id, gain);
        if let Some(c) = &self.conn {
            c.mixer.set_gain(id, gain);
        }
    }

    /// Host only: mute someone for everyone, or disconnect them. The room
    /// checks it's really the host asking.
    pub fn moderate(&self, action: &str, id: u16) {
        if let (Some(c), true, true) = (&self.conn, self.is_host, matches!(action, "mute" | "unmute" | "kick")) {
            c.send(json!({ "t": "mod", "action": action, "id": id }));
        }
    }

    pub fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
        if self.proc.as_ref().map(|p| p.id) != Some(id) || !matches!(role, Role::VoipHost | Role::VoipJoin) {
            return false;
        }
        match ev {
            TcEvent::Line(line) => {
                if role == Role::VoipHost {
                    if let (Some(addr), Some(srv), false) = (tailcat::find_addr(&line), &self.server, tailcat::is_verbose(&line)) {
                        self.code = Some(code::voip(srv.port, &addr));
                    }
                }
                let changed = self.link.observe(&line);
                if !tailcat::is_verbose(&line) || tailcat::keep_verbose(&line) {
                    self.log.push(line);
                }
                if changed {
                    self.report_status();
                }
            }
            TcEvent::Exited => {
                let why = crate::last_words(&self.log);
                self.end(format!("TailCat stopped. {why}"));
            }
        }
        true
    }

    pub fn on_event(&mut self, gen: u64, ev: VoipEvent) -> Option<Value> {
        if gen != self.gen {
            return None;
        }
        match ev {
            VoipEvent::Welcome(w) => {
                self.st = "live";
                self.me = w["you"].as_u64().unwrap_or(0) as u16;
                self.my_name = w["name"].as_str().unwrap_or(&self.my_name).to_string();
                self.room = w["room"].as_str().unwrap_or("Room").to_string();
                self.is_host = w["host"] == true;
                self.chat = w["history"].as_array().cloned().unwrap_or_default().into();
                for m in self.chat.clone() {
                    self.maybe_preview(&m);
                }
                if self.mode == Mode::Joined {
                    // First check right away, then every 30 seconds.
                    self.link.next_ping = Some(Instant::now());
                }
                None
            }
            VoipEvent::Message(m) => {
                match m["t"].as_str().unwrap_or("") {
                    "members" => {
                        self.members = m["members"].as_array().cloned().unwrap_or_default();
                        // Stop watching a share that has ended.
                        if let Some(w) = self.watching {
                            if !self.members.iter().any(|x| x["id"] == w && !x["screen"].is_null()) {
                                self.watching = None;
                                if let Some(c) = &self.conn {
                                    c.link.watch(None);
                                }
                            }
                        }
                        self.follow_share_sound();
                    }
                    "chat" => {
                        self.maybe_preview(&m);
                        if self.chat.len() >= CHAT_KEEP {
                            self.chat.pop_front();
                        }
                        self.chat.push_back(m);
                    }
                    "notice" => self.room_notice = m["text"].as_str().unwrap_or("").to_string(),
                    "keyframe" => {
                        if let Some(n) = &self.native {
                            n.share.key_frame();
                            return Some(Value::Null);
                        }
                        return Some(json!({ "type": "keyframe" }));
                    }
                    "attach.ok" | "attach.err" => {
                        let tid = m["tid"].as_u64().unwrap_or(0) as u32;
                        if let Some(u) = self.uploads.iter_mut().find(|u| u.tid == tid) {
                            u.done = true;
                            u.error = m["error"].as_str().map(str::to_string);
                        }
                        // Finished uploads leave the list; failed ones stay to show why.
                        self.uploads.retain(|u| !(u.done && u.error.is_none()));
                    }
                    "pong" => {
                        let n = m["n"].as_u64().unwrap_or(0);
                        if let Some(t) = self.pings.remove(&n) {
                            self.link.ping_result(Ok(t.elapsed()));
                            self.report_status();
                        }
                    }
                    "closed" => {
                        let why = m["reason"].as_str().unwrap_or("The room was closed.").to_string();
                        self.end(why);
                    }
                    _ => {}
                }
                None
            }
            VoipEvent::Speaking(ids) => {
                self.speaking = ids;
                Some(self.live_json(None))
            }
            VoipEvent::Level { level, talking } => Some(self.live_json(Some((level, talking)))),
            VoipEvent::Video { from, key, ts, data } => {
                if self.watching != Some(from) {
                    return Some(Value::Null);
                }
                use base64::Engine as _;
                Some(json!({
                    "type": "video", "from": from, "key": key, "ts": ts,
                    "data": base64::engine::general_purpose::STANDARD.encode(&data),
                }))
            }
            VoipEvent::Sources(r) => {
                match r {
                    Ok(list) => {
                        self.sources = list;
                        self.sources_error.clear();
                    }
                    Err(e) => {
                        self.sources.clear();
                        self.sources_error = e;
                    }
                }
                Some(json!({
                    "type": "shareSources",
                    "sources": self.sources.iter().map(|s| s.json()).collect::<Vec<_>>(),
                    "audio": crate::capture::audio_modes(),
                    "error": self.sources_error,
                }))
            }
            VoipEvent::Share(n) => {
                self.on_share(n);
                None
            }
            VoipEvent::Preview { key, ts, data } => {
                if self.native.as_ref().is_none_or(|n| !n.preview.load(Ordering::Relaxed)) {
                    return Some(Value::Null);
                }
                use base64::Engine as _;
                Some(json!({
                    "type": "preview", "key": key, "ts": ts,
                    "data": base64::engine::general_purpose::STANDARD.encode(&data),
                }))
            }
            VoipEvent::Thumb { id, thumb } => {
                use base64::Engine as _;
                let b = |v: &Option<Vec<u8>>| v.as_ref().map(|d| base64::engine::general_purpose::STANDARD.encode(d));
                Some(json!({ "type": "shareThumb", "id": id, "img": b(&thumb.image), "icon": b(&thumb.icon) }))
            }
            VoipEvent::Media(state) => {
                self.media = state;
                None
            }
            VoipEvent::VideoReport { watcher, report } => {
                let (Some(rc), true) = (self.rate.as_mut(), self.sharing.is_some()) else { return Some(Value::Null) };
                match rc.report(watcher, report, Instant::now()) {
                    Some(bitrate) => {
                        if let Some(m) = self.conn.as_ref().and_then(|c| c.media.as_ref()) {
                            m.set_rate(media::Sender::pace_for(bitrate));
                        }
                        if let Some(n) = &self.native {
                            n.share.set_bitrate(bitrate);
                            let fps = n.share.control.fps_now();
                            return Some(json!({ "type": "videoRate", "bitrate": bitrate, "fps": fps, "native": true }));
                        }
                        Some(json!({ "type": "videoRate", "bitrate": bitrate }))
                    }
                    None => Some(Value::Null),
                }
            }
            VoipEvent::Upload { tid, name, sent, size, error } => {
                if let Some(u) = self.uploads.iter_mut().find(|u| u.tid == tid) {
                    u.name = name;
                    u.sent = sent;
                    u.size = size;
                    if error.is_some() {
                        u.error = error;
                        u.done = true;
                    }
                }
                None
            }
            VoipEvent::Fetching { aid, got, size } => {
                if let Some(d) = self.downloads.get_mut(&aid) {
                    d.got = got;
                    d.size = size;
                }
                None
            }
            VoipEvent::Fetched { aid, result, preview } => {
                if preview {
                    self.previews.insert(aid, result.is_ok());
                } else if let Some(d) = self.downloads.get_mut(&aid) {
                    match result {
                        Ok(p) => {
                            d.st = "done";
                            d.path = Some(p);
                        }
                        Err(e) => {
                            d.st = "error";
                            d.error = e;
                        }
                    }
                }
                None
            }
            VoipEvent::Failed(why) => {
                if self.st == "starting" {
                    self.reset();
                    self.st = "off";
                    self.error = why;
                } else {
                    self.end(why);
                }
                None
            }
        }
    }

    /// The small, frequent update: who's talking and the mic meter.
    fn live_json(&self, level: Option<(f32, bool)>) -> Value {
        json!({
            "type": "voipLive",
            "speaking": self.speaking,
            "me": self.me,
            "level": level.map(|l| l.0),
            "talking": level.map(|l| l.1),
        })
    }

    /// Tells the room how this member is connected, for everyone's list.
    fn report_status(&self) {
        if self.mode != Mode::Joined {
            return;
        }
        if let Some(c) = &self.conn {
            let l = self.link.json();
            c.send(json!({ "t": "status", "path": l["path"], "rtt": l["rtt"] }));
        }
    }

    /// Pings the room every 30 seconds; two misses after it has answered means
    /// it's gone. Returns true if anything changed.
    pub fn tick(&mut self) -> bool {
        if self.mode != Mode::Joined || !self.is_live() {
            return false;
        }
        let now = Instant::now();
        let mut changed = false;
        let stale: Vec<u64> = self.pings.iter().filter(|(_, t)| t.elapsed() > Duration::from_secs(15)).map(|(n, _)| *n).collect();
        for n in stale {
            self.pings.remove(&n);
            changed = true;
            if self.link.ping_result(Err("timeout".into())) {
                self.end("The room stopped answering. The host may have closed it or gone offline.".into());
                return true;
            }
        }
        if self.pings.is_empty() && self.link.ping_due(now) {
            if let Some(c) = &self.conn {
                self.ping_n += 1;
                self.pings.insert(self.ping_n, now);
                self.link.ping_started();
                c.send(json!({ "t": "ping", "n": self.ping_n }));
            }
        }
        changed
    }

    pub fn shutdown(&mut self) {
        self.reset();
    }

    /// For the end-to-end check: events from two rooms in one process are
    /// told apart by generation.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_gen_base(&mut self, base: u64) {
        self.gen = base;
    }

    /// For the end-to-end check: a room message, as the page would send it.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_send(&self, v: Value) {
        if let Some(c) = &self.conn {
            c.send(v);
        }
    }

    /// For the end-to-end check: one voice packet, exactly as the audio
    /// engine hands it to the media sender (there's no microphone here).
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_voice(&self, seq: u32, opus: &[u8]) {
        if let Some(m) = self.conn.as_ref().and_then(|c| c.media.as_ref()) {
            m.urgent(media::voice(seq, opus));
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "st": self.st,
            "mode": self.mode_name(),
            "error": self.error,
            "notice": self.notice,
            "room": self.room,
            "name": self.my_name,
            "me": self.me,
            "code": self.code,
            "members": self.members,
            "chat": self.chat,
            "speaking": self.speaking,
            "inVoice": self.in_voice,
            "muted": self.muted,
            "deaf": self.deaf,
            "voiceError": self.voice_error,
            "media": self.media,
            "devices": self.devices,
            "link": self.link.json(),
            "log": self.log.json(),
            "gen": self.gen,
            "isHost": self.is_host,
            "roomNotice": self.room_notice,
            "sharing": self.sharing,
            "sharingNative": self.native.is_some(),
            "shareStarting": self.native.as_ref().is_some_and(|n| n.info.is_none()),
            "shareError": self.share_error,
            "shareNotice": self.share_notice,
            "shareVolume": self.share_volume,
            "watching": self.watching,
            "uploads": self.uploads.iter().map(|u| json!({
                "tid": u.tid, "name": u.name, "sent": u.sent, "size": u.size, "error": u.error,
            })).collect::<Vec<_>>(),
            "downloads": self.downloads.iter().map(|(aid, d)| (aid.to_string(), json!({
                "st": d.st, "got": d.got, "size": d.size, "error": d.error,
            }))).collect::<serde_json::Map<_, _>>(),
            "previews": self.previews.iter().filter(|(_, ok)| **ok).map(|(aid, _)| *aid).collect::<Vec<_>>(),
            "gains": self.gains.iter().map(|(id, g)| (id.to_string(), json!(g))).collect::<serde_json::Map<_, _>>(),
        })
    }
}
