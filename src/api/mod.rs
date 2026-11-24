//! The game API: games (and other programs) on this PC can use Portside.
//!
//! It listens on 127.0.0.1 only, at the first free port from 47800 to 47809,
//! so nothing on the network can reach it, and it's off until turned on in
//! Settings. The first time a program connects, Portside asks whether to
//! allow it; once allowed it gets a key, which only works for that program.
//!
//! An allowed game can see the hub you're in (who's in it, what they're
//! playing), the hubs saved on this PC and your voice channel, say what
//! you're playing, send small messages to the same game on friends' PCs, and
//! host or join a game session: its own tunnel, made with the VOIP helper
//! (the TailCat copy that carries UDP as well as TCP) and handed only to the
//! hub, so a game's UDP and TCP traffic between friends goes inside an
//! encrypted WireGuard tunnel like everything else in Portside. The game
//! itself only ever talks to 127.0.0.1.
//!
//! One JSON object per line, each way. See docs/game-api.md.
//!
//!   ← {"op":"portside","v":1,"version":"0.16.0"}
//!   → {"op":"hello","v":1,"app":{"id":"my-game","name":"My Game"},"token":"…"}   (no token the first time)
//!   ← {"op":"asking"}  (Portside is asking you)  then  ← {"op":"authorized","token":"…"}  or  ← {"op":"denied",…}
//!   ← {"op":"ready","v":1,"app":"my-game"}
//!   → {"id":1,"cmd":"getHub"}   ← {"id":1,"ok":true,"data":{…}}   (or "ok":false,"error":"…")
//!   ← {"evt":"hub","data":{…}}   (and "voice", "speaking", "sessions", "message", "sessionEnded")

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::RangeInclusive;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::hub::clean;
use crate::tailcat::{self, Link, Proc, Role, TcEvent};
use crate::{Log, UserEvent};

mod peer;
mod server;
mod sha256;
#[cfg(test)]
mod tests;

pub use server::ApiServer;

/// The ports tried, in order; a game looks through the same ones.
pub const PORTS: RangeInclusive<u16> = 47800..=47809;
/// The protocol's version.
pub const VERSION: u64 = 1;
/// The most a session's info may be, as JSON.
pub const MAX_INFO: usize = 2048;
const MAX_LINE: u64 = 64 * 1024;
const MAX_CONNS: usize = 16;
/// How long a program waits for you to answer before it's turned away.
const ASK_FOR: Duration = Duration::from_secs(120);
/// After "Don't allow", the same game can't ask again for this long.
const DENIED_FOR: Duration = Duration::from_secs(60);
/// Sessions one program can host, and join, at once.
const MAX_HOSTED: usize = 4;
const MAX_JOINED: usize = 8;
const MAX_APPS: usize = 64;

pub type Emit = Arc<dyn Fn(ApiEvent) + Send + Sync>;

/// What the listener's threads hand the app.
pub enum ApiEvent {
    /// A program connected and said hello; `exe` is its file, if the OS said.
    Open { conn: u64, tx: Sender<Value>, exe: Option<String>, hello: Value },
    Msg { conn: u64, msg: Value },
    Closed { conn: u64 },
}

/// A game id: lowercase letters, digits, dots, dashes and underscores,
/// up to 64, starting with a letter or digit ("com.kit.space-game").
pub fn app_id(s: &str) -> Option<String> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    ok.then(|| s.to_string())
}

/// A program allowed to use the API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Allowed {
    pub id: String,
    pub name: String,
    /// The program's file when it was allowed; its key works only from there.
    pub exe: Option<String>,
    /// SHA-256 of its key (the key itself is never kept).
    pub hash: String,
    pub added: u64,
    pub used: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    pub apps: Vec<Allowed>,
}

fn settings_path() -> std::path::PathBuf {
    crate::portable::data_dir().join("settings.json")
}

pub fn load() -> Config {
    std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| serde_json::from_value::<Config>(v["gameApi"].clone()).ok())
        .unwrap_or_default()
}

fn save(c: &Config) {
    let path = settings_path();
    let mut v = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or(json!({}));
    v["gameApi"] = json!(c);
    let tmp = path.with_extension("json.gameapi.part");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn now() -> u64 {
    crate::hubstore::now()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    /// Waiting for you to allow it.
    Asking,
    Ready,
}

struct Conn {
    tx: Sender<Value>,
    exe: Option<String>,
    id: String,
    name: String,
    st: St,
    since: Instant,
    /// The last of each event sent, so only changes go.
    sent: HashMap<&'static str, Value>,
    /// What it says we're doing ("In a lobby"), and when it said so.
    activity: Option<(String, Instant)>,
}

impl Conn {
    fn send(&self, v: Value) {
        let _ = self.tx.send(v);
    }
}

/// A session a game on this PC hosts: `kc-voip serve` on the game's port.
pub struct Hosted {
    pub sid: String,
    conn: u64,
    pub app: String,
    pub title: String,
    pub info: Value,
    pub port: u16,
    proc: Option<Proc>,
    pub addr: Option<String>,
    /// The request waiting for the tunnel to be up.
    pending: Option<Value>,
    pub link: Link,
    log: Log,
}

/// A session a game on this PC joined: `kc-voip forward` to its host, on a
/// port of this PC for the game to use.
pub struct Joined {
    /// "<host's member id>:<sid>"
    pub id: String,
    conn: u64,
    pub app: String,
    pub title: String,
    pub host: String,
    pub local: u16,
    proc: Option<Proc>,
    pub ready: bool,
    pending: Option<Value>,
    pub link: Link,
    log: Log,
}

/// A session in the hub, as a game joins it.
pub struct Target {
    pub id: String,
    pub app: String,
    pub title: String,
    pub host: String,
    pub port: u16,
    pub addr: String,
}

pub struct Api {
    pub config: Config,
    server: Option<ApiServer>,
    pub error: String,
    conns: BTreeMap<u64, Conn>,
    /// Programs waiting for you, oldest first.
    asking: VecDeque<u64>,
    denied: HashMap<String, Instant>,
    pub hosted: Vec<Hosted>,
    pub joined: Vec<Joined>,
    /// A new "Allow?" to bring the window up for.
    attention: bool,
    /// Who's talking, as last sent.
    speaking: Value,
    emit: Emit,
}

/// A reply to request `id`.
pub fn ok(id: &Value, data: Value) -> Value {
    json!({ "id": id, "ok": true, "data": data })
}

pub fn err(id: &Value, error: impl Into<String>) -> Value {
    json!({ "id": id, "ok": false, "error": error.into() })
}

fn event(name: &str, data: Value) -> Value {
    json!({ "evt": name, "data": data })
}

impl Api {
    pub fn new(config: Config, emit: Emit) -> Api {
        let mut api = Api {
            config,
            server: None,
            error: String::new(),
            conns: BTreeMap::new(),
            asking: VecDeque::new(),
            denied: HashMap::new(),
            hosted: Vec::new(),
            joined: Vec::new(),
            attention: false,
            speaking: Value::Null,
            emit,
        };
        api.apply();
        api
    }

    /// Listening when it's on, not when it's off.
    fn apply(&mut self) {
        if !self.config.enabled {
            self.server = None;
            for c in std::mem::take(&mut self.conns).into_values() {
                c.send(Value::Null);
            }
            self.asking.clear();
            self.hosted.clear();
            self.joined.clear();
            self.error.clear();
            return;
        }
        if self.server.is_none() {
            match ApiServer::start(self.emit.clone()) {
                Ok(s) => {
                    self.server = Some(s);
                    self.error.clear();
                }
                Err(e) => self.error = e,
            }
        }
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.config.enabled = on;
        save(&self.config);
        self.apply();
    }

    pub fn port(&self) -> Option<u16> {
        self.server.as_ref().map(|s| s.port)
    }

    /// Once: a new "Allow?" is waiting, so the window should come up.
    pub fn take_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }

    /// A program said hello: it's let in with a key that's its own, asks
    /// you, or is turned away.
    pub fn open(&mut self, conn: u64, tx: Sender<Value>, exe: Option<String>, hello: &Value) {
        let close = |tx: &Sender<Value>, why: &str| {
            let _ = tx.send(json!({ "op": "denied", "error": why }));
            let _ = tx.send(Value::Null);
        };
        if !self.config.enabled {
            return close(&tx, "The game API is off in Portside's Settings.");
        }
        let Some(id) = app_id(hello["app"]["id"].as_str().unwrap_or_default()) else {
            return close(&tx, "app.id must be 1 to 64 lowercase letters, digits, dots, dashes or underscores.");
        };
        let mut name = clean(hello["app"]["name"].as_str().unwrap_or_default(), 48);
        if name.is_empty() {
            name = id.clone();
        }
        let token = hello["token"].as_str().unwrap_or_default();
        let c = Conn { tx, exe, id: id.clone(), name, st: St::Asking, since: Instant::now(), sent: HashMap::new(), activity: None };
        if !token.is_empty() {
            let hash = sha256::hex(token.as_bytes());
            let found = self.config.apps.iter_mut().find(|a| a.id == id && crate::hub::same_secret(&a.hash, &hash));
            match found {
                // Its key, from the program it was given to (or a program the OS didn't name either time).
                Some(a) if a.exe.is_none() || c.exe.is_none() || a.exe == c.exe => {
                    a.used = now();
                    if c.exe.is_some() && a.exe.is_none() {
                        a.exe = c.exe.clone();
                    }
                    save(&self.config);
                    self.conns.insert(conn, Conn { st: St::Ready, ..c });
                    self.ready(conn);
                    return;
                }
                // The right key from another program, or a key that isn't
                // (removed in Settings, say): ask, as the first time.
                _ => {}
            }
        }
        if self.denied.get(&id).is_some_and(|t| t.elapsed() < DENIED_FOR) {
            return close(&c.tx, "You chose not to allow this. Try again in a minute.");
        }
        if self.asking.len() >= 8 {
            return close(&c.tx, "Too many programs are waiting to be allowed.");
        }
        c.send(json!({ "op": "asking", "text": "Portside is asking whether to allow this. Answer in the Portside window." }));
        self.conns.insert(conn, c);
        self.asking.push_back(conn);
        self.attention = true;
    }

    fn ready(&mut self, conn: u64) {
        if let Some(c) = self.conns.get(&conn) {
            c.send(json!({ "op": "ready", "v": VERSION, "app": c.id, "version": crate::VERSION }));
        }
    }

    /// You allowed the program asking (and any others asking as the same
    /// game from the same file): each gets the key.
    pub fn allow(&mut self, conn: u64) {
        let Some(c) = self.conns.get(&conn).filter(|c| c.st == St::Asking) else { return };
        let (id, name, exe) = (c.id.clone(), c.name.clone(), c.exe.clone());
        let token = crate::peers::secret();
        self.config.apps.retain(|a| a.id != id);
        self.config.apps.push(Allowed { id: id.clone(), name, exe: exe.clone(), hash: sha256::hex(token.as_bytes()), added: now(), used: now() });
        if self.config.apps.len() > MAX_APPS {
            self.config.apps.remove(0);
        }
        save(&self.config);
        let same: Vec<u64> = self.asking.iter().copied().filter(|k| self.conns.get(k).is_some_and(|o| o.id == id && o.exe == exe)).collect();
        self.asking.retain(|k| !same.contains(k));
        for k in same {
            if let Some(o) = self.conns.get_mut(&k) {
                o.st = St::Ready;
                o.send(json!({ "op": "authorized", "token": token }));
            }
            self.ready(k);
        }
    }

    /// You said no: everyone asking as that game is turned away, and it
    /// can't ask again for a minute.
    pub fn deny(&mut self, conn: u64) {
        let Some(id) = self.conns.get(&conn).filter(|c| c.st == St::Asking).map(|c| c.id.clone()) else { return };
        self.denied.insert(id.clone(), Instant::now());
        let same: Vec<u64> = self.asking.iter().copied().filter(|k| self.conns.get(k).is_some_and(|o| o.id == id)).collect();
        for k in same {
            self.turn_away(k, "You chose not to allow this program in Portside.");
        }
    }

    fn turn_away(&mut self, conn: u64, why: &str) {
        self.asking.retain(|k| *k != conn);
        if let Some(c) = self.conns.remove(&conn) {
            c.send(json!({ "op": "denied", "error": why }));
            c.send(Value::Null);
        }
        self.end_conn(conn);
    }

    /// Takes a program off the allowed list; if it's connected, it's
    /// disconnected (and what it hosted or joined ends).
    pub fn remove(&mut self, id: &str) {
        self.config.apps.retain(|a| a.id != id);
        save(&self.config);
        let gone: Vec<u64> = self.conns.iter().filter(|(_, c)| c.id == id).map(|(k, _)| *k).collect();
        for k in gone {
            self.turn_away(k, "Removed from Portside's allowed programs.");
        }
    }

    /// The program disconnected.
    pub fn closed(&mut self, conn: u64) {
        self.asking.retain(|k| *k != conn);
        self.conns.remove(&conn);
        self.end_conn(conn);
    }

    /// What a program hosted or joined ends with it.
    fn end_conn(&mut self, conn: u64) {
        self.hosted.retain(|h| h.conn != conn);
        self.joined.retain(|j| j.conn != conn);
    }

    /// Once a second: programs nobody answered for are turned away.
    pub fn tick(&mut self) -> bool {
        let late: Vec<u64> = self.asking.iter().copied().filter(|k| self.conns.get(k).is_some_and(|c| c.since.elapsed() > ASK_FOR)).collect();
        for k in &late {
            self.turn_away(*k, "Nobody answered in Portside.");
        }
        !late.is_empty()
    }

    /// A request from a ready program: (its game id, its name), or None if
    /// it isn't ready (it's told so).
    pub fn who(&self, conn: u64, req: &Value) -> Option<(String, String)> {
        let c = self.conns.get(&conn)?;
        if c.st != St::Ready {
            c.send(err(&req["id"], "Not allowed yet: wait for \"ready\"."));
            return None;
        }
        Some((c.id.clone(), c.name.clone()))
    }

    pub fn reply(&self, conn: u64, v: Value) {
        if let Some(c) = self.conns.get(&conn) {
            c.send(v);
        }
    }

    /// Something ready programs can see changed: each hears it if it's new
    /// to them. `per_app` gives a value for each game id.
    pub fn push(&mut self, name: &'static str, value: impl Fn(&str) -> Value) {
        for c in self.conns.values_mut().filter(|c| c.st == St::Ready) {
            let v = value(&c.id);
            if c.sent.get(name) != Some(&v) {
                c.send(event(name, v.clone()));
                c.sent.insert(name, v);
            }
        }
    }

    pub fn has_ready(&self) -> bool {
        self.conns.values().any(|c| c.st == St::Ready)
    }

    /// Who's talking in voice, many times a second: sent only when it changes.
    pub fn speaking(&mut self, ids: &Value) {
        if *ids == self.speaking {
            return;
        }
        self.speaking = ids.clone();
        for c in self.conns.values().filter(|c| c.st == St::Ready) {
            c.send(event("speaking", json!({ "ids": ids })));
        }
    }

    /// A game message from the hub, to this game on this PC.
    pub fn deliver(&self, msg: &Value) {
        let Some(app) = app_id(msg["app"].as_str().unwrap_or_default()) else { return };
        let from = json!({ "id": msg["from"], "name": clean(msg["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) });
        for c in self.conns.values().filter(|c| c.st == St::Ready && c.id == app) {
            c.send(event("message", json!({ "from": from, "data": msg["data"] })));
        }
    }

    /// What a program says we're doing.
    pub fn set_activity(&mut self, conn: u64, text: Option<String>) {
        if let Some(c) = self.conns.get_mut(&conn) {
            c.activity = text.map(|t| (t, Instant::now()));
        }
    }

    /// The activity to show beside our name: the latest any program set.
    pub fn activity(&self) -> Option<(String, String)> {
        self.conns
            .values()
            .filter(|c| c.st == St::Ready)
            .filter_map(|c| c.activity.as_ref().map(|(t, at)| (at, c.name.clone(), t.clone())))
            .max_by_key(|(at, _, _)| **at)
            .map(|(_, n, t)| (n, t))
    }

    /// Our games' sessions, once their tunnels are up, for the hub:
    /// (sid, game id, title, port, address, info).
    pub fn announce(&self) -> Vec<(String, String, String, u16, String, Value)> {
        self.hosted
            .iter()
            .filter_map(|h| Some((h.sid.clone(), h.app.clone(), h.title.clone(), h.port, h.addr.clone()?, h.info.clone())))
            .collect()
    }

    /// Hosts a session: the VOIP helper serves the game's port (TCP and
    /// UDP, on 127.0.0.1) through a new tunnel. The reply waits for it.
    pub fn host(&mut self, conn: u64, req: &Value, app: &str, proxy: &EventLoopProxy<UserEvent>) {
        let id = &req["id"];
        let Some(port) = req["port"].as_u64().filter(|p| (1024..=65535).contains(p)).map(|p| p as u16) else {
            return self.reply(conn, err(id, "port must be the game's own port, 1024 to 65535."));
        };
        if PORTS.contains(&port) {
            return self.reply(conn, err(id, "That's Portside's own port."));
        }
        if self.hosted.iter().filter(|h| h.conn == conn).count() >= MAX_HOSTED {
            return self.reply(conn, err(id, format!("One program can host {MAX_HOSTED} sessions at once.")));
        }
        if self.hosted.iter().any(|h| h.port == port) {
            return self.reply(conn, err(id, "A session is already hosted on that port."));
        }
        let info = req.get("info").cloned().unwrap_or(Value::Null);
        if info.to_string().len() > MAX_INFO {
            return self.reply(conn, err(id, format!("info must be at most {MAX_INFO} bytes of JSON.")));
        }
        let args = vec!["serve".into(), "--key=new".into(), port.to_string()];
        let proc = match tailcat::spawn(Role::GameHost, &args, proxy) {
            Ok(p) => p,
            Err(e) => return self.reply(conn, err(id, e)),
        };
        let sid = format!("{:08x}", crate::peers::new_id() as u32);
        self.hosted.push(Hosted {
            sid,
            conn,
            app: app.to_string(),
            title: clean(req["title"].as_str().unwrap_or_default(), 60),
            info,
            port,
            proc: Some(proc),
            addr: None,
            pending: Some(id.clone()),
            link: Link::default(),
            log: Log::default(),
        });
    }

    /// A hosted session's new title or info: the hub hears on the next tick.
    pub fn update_hosting(&mut self, conn: u64, req: &Value) {
        let sid = req["sid"].as_str().unwrap_or_default();
        let id = &req["id"];
        if req.get("info").is_some_and(|i| i.to_string().len() > MAX_INFO) {
            return self.reply(conn, err(id, format!("info must be at most {MAX_INFO} bytes of JSON.")));
        }
        let Some(h) = self.hosted.iter_mut().find(|h| h.conn == conn && h.sid == sid) else {
            return self.reply(conn, err(id, "No session of yours has that sid."));
        };
        if let Some(t) = req["title"].as_str() {
            h.title = clean(t, 60);
        }
        if let Some(i) = req.get("info") {
            h.info = i.clone();
        }
        self.reply(conn, ok(id, json!({})));
    }

    pub fn stop_hosting(&mut self, conn: u64, req: &Value) {
        let sid = req["sid"].as_str().unwrap_or_default();
        let had = self.hosted.len();
        self.hosted.retain(|h| !(h.conn == conn && h.sid == sid));
        let r = if self.hosted.len() < had { ok(&req["id"], json!({})) } else { err(&req["id"], "No session of yours has that sid.") };
        self.reply(conn, r);
    }

    /// Joins a session in the hub: the VOIP helper forwards a port of this
    /// PC (TCP and UDP, 127.0.0.1) to the host's game through its tunnel.
    pub fn join(&mut self, conn: u64, req: &Value, t: Target, proxy: &EventLoopProxy<UserEvent>) {
        let id = &req["id"];
        if let Some(j) = self.joined.iter().find(|j| j.conn == conn && j.id == t.id) {
            let r = if j.ready { ok(id, json!({ "id": j.id, "port": j.local })) } else { err(id, "Already joining it.") };
            return self.reply(conn, r);
        }
        if self.joined.iter().filter(|j| j.conn == conn).count() >= MAX_JOINED {
            return self.reply(conn, err(id, format!("One program can join {MAX_JOINED} sessions at once.")));
        }
        let local = match crate::free_port() {
            Ok(p) => p,
            Err(e) => return self.reply(conn, err(id, e)),
        };
        let args = vec!["forward".into(), t.addr.clone(), format!("{local}:{}", t.port)];
        let proc = match tailcat::spawn(Role::GameJoin, &args, proxy) {
            Ok(p) => p,
            Err(e) => return self.reply(conn, err(id, e)),
        };
        self.joined.push(Joined {
            id: t.id,
            conn,
            app: t.app,
            title: t.title,
            host: t.host,
            local,
            proc: Some(proc),
            ready: false,
            pending: Some(id.clone()),
            link: Link::default(),
            log: Log::default(),
        });
    }

    pub fn leave(&mut self, conn: u64, req: &Value) {
        let sid = req["session"].as_str().unwrap_or_default();
        let had = self.joined.len();
        self.joined.retain(|j| !(j.conn == conn && j.id == sid));
        let r = if self.joined.len() < had { ok(&req["id"], json!({})) } else { err(&req["id"], "You haven't joined that session.") };
        self.reply(conn, r);
    }

    /// What a session's tunnel said.
    pub fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
        let current = |p: &Option<Proc>| p.as_ref().map(|p| p.id) == Some(id);
        match role {
            Role::GameHost => {
                let Some(i) = self.hosted.iter().position(|h| current(&h.proc)) else { return false };
                let h = &mut self.hosted[i];
                match ev {
                    TcEvent::Line(line) => {
                        if let Some(addr) = tailcat::find_addr(&line).filter(|_| !tailcat::is_verbose(&line) && h.addr.is_none()) {
                            h.addr = Some(addr);
                            if let Some(rid) = h.pending.take() {
                                let r = ok(&rid, json!({ "sid": h.sid, "port": h.port }));
                                if let Some(c) = self.conns.get(&h.conn) {
                                    c.send(r);
                                }
                            }
                        }
                        crate::tc_line(&mut h.log, &mut h.link, line);
                    }
                    TcEvent::Exited => {
                        let h = self.hosted.remove(i);
                        let why = format!("The session's tunnel stopped. {}", crate::last_words(&h.log));
                        if let Some(c) = self.conns.get(&h.conn) {
                            c.send(match h.pending {
                                Some(rid) => err(&rid, why),
                                None => event("sessionEnded", json!({ "sid": h.sid, "error": why })),
                            });
                        }
                    }
                }
            }
            Role::GameJoin => {
                let Some(i) = self.joined.iter().position(|j| current(&j.proc)) else { return false };
                let j = &mut self.joined[i];
                match ev {
                    TcEvent::Line(line) => {
                        // Listening on this PC, UDP too: the game can start sending.
                        if !j.ready && line.contains("forwarding udp") {
                            j.ready = true;
                            if let Some(rid) = j.pending.take() {
                                let r = ok(&rid, json!({ "id": j.id, "port": j.local }));
                                if let Some(c) = self.conns.get(&j.conn) {
                                    c.send(r);
                                }
                            }
                        }
                        crate::tc_line(&mut j.log, &mut j.link, line);
                    }
                    TcEvent::Exited => {
                        let j = self.joined.remove(i);
                        let why = format!("The session's tunnel stopped. {}", crate::last_words(&j.log));
                        if let Some(c) = self.conns.get(&j.conn) {
                            c.send(match j.pending {
                                Some(rid) => err(&rid, why),
                                None => event("sessionEnded", json!({ "id": j.id, "error": why })),
                            });
                        }
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// For Settings: on or off, where, who's allowed (and connected), who's
    /// asking, and the sessions running.
    pub fn json(&self) -> Value {
        let prompt = self.asking.front().and_then(|k| self.conns.get(k).map(|c| (k, c))).map(|(k, c)| {
            let known = self.config.apps.iter().find(|a| a.id == c.id);
            json!({
                "conn": k, "id": c.id, "name": c.name,
                "exe": c.exe, "exeName": c.exe.as_deref().map(peer::file_name),
                // Allowed before, from another program file: say so.
                "otherExe": known.is_some_and(|a| a.exe.is_some() && c.exe.is_some() && a.exe != c.exe),
                "waiting": self.asking.len(),
            })
        });
        json!({
            "enabled": self.config.enabled,
            "port": self.port(),
            "error": self.error,
            "apps": self.config.apps.iter().rev().map(|a| json!({
                "id": a.id, "name": a.name, "exe": a.exe, "added": a.added, "used": a.used,
                "connected": self.conns.values().filter(|c| c.st == St::Ready && c.id == a.id).count(),
            })).collect::<Vec<_>>(),
            "prompt": prompt,
            "sessions": self.hosted.iter().map(|h| json!({
                "kind": "host", "app": self.name_of(&h.app), "title": h.title, "port": h.port, "live": h.addr.is_some(),
            })).chain(self.joined.iter().map(|j| json!({
                "kind": "join", "app": self.name_of(&j.app), "title": j.title, "host": j.host, "port": j.local,
                "live": j.ready, "path": j.link.path(),
            }))).collect::<Vec<_>>(),
        })
    }

    fn name_of(&self, app: &str) -> String {
        self.config.apps.iter().find(|a| a.id == app).map(|a| a.name.clone()).unwrap_or_else(|| app.to_string())
    }
}
