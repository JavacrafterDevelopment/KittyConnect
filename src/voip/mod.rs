//! VOIP: a temporary room with a text channel and a voice channel, hosted on
//! one PC and joined through a TailCat code. Names, messages and voice only
//! ever travel inside the encrypted tunnel (or over loopback, for the host).

pub mod adpcm;
pub mod audio;
pub mod client;
pub mod server;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
}

const CHAT_KEEP: usize = 500;

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
        }
    }
}

fn clean_name(name: &str) -> Result<String, String> {
    let n = server::clean(name, 32);
    if n.is_empty() {
        Err("Type your name first.".into())
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
        let srv = match server::RoomServer::start(room) {
            Ok(s) => s,
            Err(e) => return self.error = e,
        };
        let args = vec!["serve".into(), "--key=new".into(), srv.port.to_string()];
        match tailcat::spawn(Role::VoipHost, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        self.conn = Some(client::connect(srv.port, name.clone(), Some(srv.host_token.clone()), self.gen, proxy.clone()));
        self.server = Some(srv);
        self.my_name = name;
        self.mode = Mode::Hosting;
        self.st = "starting";
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
            Ok(_) => return self.error = "That's a port code. Ask for the room code (it starts with kitty:voip:).".into(),
            Err(e) => return self.error = e,
        };
        let local = match crate::free_port() {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        match tailcat::spawn(Role::VoipJoin, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        self.conn = Some(client::connect(local, name.clone(), None, self.gen, proxy.clone()));
        self.code = Some(code::voip(c.ports[0], &c.addr));
        self.my_name = name;
        self.mode = Mode::Joined;
        self.st = "starting";
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
        match audio::Engine::start(conn.mixer.clone(), conn.tx.clone(), self.muted) {
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
            c.send(json!({ "t": "mute", "muted": self.muted, "deaf": deaf }));
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
                self.chat = w["history"].as_array().cloned().unwrap_or_default().into();
                if self.mode == Mode::Joined {
                    // First check right away, then every 30 seconds.
                    self.link.next_ping = Some(Instant::now());
                }
                None
            }
            VoipEvent::Message(m) => {
                match m["t"].as_str().unwrap_or("") {
                    "members" => self.members = m["members"].as_array().cloned().unwrap_or_default(),
                    "chat" => {
                        if self.chat.len() >= CHAT_KEEP {
                            self.chat.pop_front();
                        }
                        self.chat.push_back(m);
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

    pub fn json(&self) -> Value {
        json!({
            "st": self.st,
            "mode": match self.mode { Mode::Off => "off", Mode::Hosting => "host", Mode::Joined => "join" },
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
            "devices": self.devices,
            "link": self.link.json(),
            "log": self.log.json(),
        })
    }
}
