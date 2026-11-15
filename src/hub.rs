//! The Hub: one code for a group of friends.
//!
//! Whoever hosts the hub runs a small server on this PC (127.0.0.1 and ::1
//! only) and serves its one port through the official TailCat, with a new key
//! and pre-shared key each time: `tailcat serve --key=new <port>`. Friends join
//! with the hub code, `kitty:hub:<port>:<address>`, through the official
//! TailCat too (`tailcat forward`), and each keeps one connection to the hub
//! inside that encrypted tunnel. The host's own KittyConnect connects to its
//! server directly, with a secret only it knows, which makes it the host.
//!
//! Over that connection everyone sees who is in the hub (and whether each is
//! direct or on a relay), and anyone can invite others to something they
//! opened elsewhere in the app: a voice room, a file share, ports, a game
//! server or a shared folder. The invite carries that thing's code, so nobody
//! copies and pastes it; the room or folder itself still connects the way it
//! always does (a voice room through the VOIP helper, the rest through the
//! official TailCat). Nothing is joined until the person invited presses Join.
//!
//! One JSON object per line, each way:
//!
//!   → {"op":"hello","v":1,"peer":<random>,"name":"Sam","path":"direct","rtt":20[,"host":<secret>]}
//!   ← {"op":"welcome","v":1,"you":3,"hub":"Kit's hub"}  (or {"op":"full"} / {"op":"old"})
//!   ← {"op":"members","list":[{"id":1,"name":"Kit","host":true,"path":"host"},…]}
//!   → {"op":"path","path":"relay","rtt":80}
//!   → {"op":"invite","to":[3,4],"code":"kitty:voip:…","title":"Kit's room"}   ([] for everyone)
//!   ← {"op":"invite","inv":7,"from":1,"name":"Kit","kind":"voip","code":"kitty:voip:…","title":"Kit's room"}
//!   ← {"op":"invited","n":2}
//!   → {"op":"kick","id":4}                                   (host only)
//!   ← {"op":"kicked"}  (then the hub closes the connection)
//!   → {"op":"ping"}  ← {"op":"pong"}                          (every 15 s)
//!   → {"op":"bye"}

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::code::{self, Kind};
use crate::tailcat::{self, Link, Proc, Role, TcEvent};
use crate::{Log, UserEvent};

pub const PROTOCOL: u64 = 1;
/// The most people in one hub.
pub const MAX_MEMBERS: usize = 32;
/// The longest line either side takes.
const MAX_LINE: u64 = 8 * 1024;
const MAX_TITLE: usize = 60;
/// Invites one person may send a minute; more are refused.
const INVITES_PER_MINUTE: usize = 12;
/// Each side pings every this often, and gives up after a few missed.
const PING_EVERY: Duration = Duration::from_secs(15);
const SILENT_FOR: Duration = Duration::from_secs(50);
/// A friend's hello can take a while: the tunnel comes up first.
const HELLO_WAIT: Duration = Duration::from_secs(25);

/// Printable, trimmed, length-capped (names, hub names, invite titles).
pub fn clean(s: &str, max: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    s.trim().chars().take(max).collect::<String>().trim().to_string()
}

/// What an invite is to, from its code, and the code written out clean
/// (nothing but the code travels): only codes for things a hub can invite
/// to. Port codes need their port numbers in them; a bare address can't be
/// joined without being told them.
pub fn invitation(code_text: &str) -> Option<(&'static str, String)> {
    let c = code::parse(code_text).ok()?;
    let kind = match c.kind {
        Kind::Voip => "voip",
        Kind::File => "file",
        Kind::Game => "game",
        Kind::Mount => "mount",
        Kind::Port => "port",
        Kind::Bare | Kind::Hub => return None,
    };
    Some((kind, code::canonical(&c)?))
}

#[cfg(test)]
fn invite_kind(code_text: &str) -> Option<&'static str> {
    invitation(code_text).map(|(k, _)| k)
}

/// Two secrets compared without stopping at the first difference.
pub fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Writes one line; false if the connection is gone.
fn send_line(stream: &mut TcpStream, v: &Value) -> bool {
    let mut line = v.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).and_then(|_| stream.flush()).is_ok()
}

/// Reads one line of at most MAX_LINE bytes. None at the end, on an error,
/// on a timeout, or if the line is too long.
fn read_line(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut buf = Vec::new();
    let n = reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf).ok()?;
    if n == 0 || buf.last() != Some(&b'\n') {
        return None;
    }
    serde_json::from_slice(&buf).ok().or(Some(Value::Null))
}

// ------------------------------------------------------------------ server

struct Member {
    id: u64,
    /// A random number the member picked, so a reconnect replaces its old
    /// entry instead of listing it twice. Never shown to anyone.
    peer: u64,
    name: String,
    host: bool,
    path: &'static str,
    rtt: Option<u32>,
    out: Sender<Value>,
    stream: TcpStream,
    invites: VecDeque<Instant>,
}

struct State {
    name: String,
    members: Vec<Member>,
    next_id: u64,
    next_inv: u64,
}

impl State {
    fn list(&self) -> Value {
        json!(self
            .members
            .iter()
            .map(|m| json!({ "id": m.id, "name": m.name, "host": m.host, "path": m.path, "rtt": m.rtt }))
            .collect::<Vec<_>>())
    }

    fn broadcast_members(&self) {
        let msg = json!({ "op": "members", "list": self.list() });
        for m in &self.members {
            let _ = m.out.send(msg.clone());
        }
    }

    /// Takes someone out: their connection closes, everyone else hears.
    fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.members.iter().position(|m| m.id == id) else { return false };
        let m = self.members.remove(i);
        let _ = m.stream.shutdown(Shutdown::Both);
        self.broadcast_members();
        true
    }
}

/// A hub running on this PC.
pub struct HubServer {
    pub port: u16,
    /// Proves the host's own KittyConnect is the host. Never leaves this PC.
    pub host_token: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl HubServer {
    pub fn start(name: &str) -> Result<HubServer, String> {
        let (v4, v6) = crate::loopback::listen().map_err(|e| format!("Couldn't open the hub: {e}"))?;
        let port = v4.local_addr().map_err(|e| e.to_string())?.port();
        let state = Arc::new(Mutex::new(State { name: clean(name, 48), members: Vec::new(), next_id: 1, next_inv: 1 }));
        let stop = Arc::new(AtomicBool::new(false));
        let host_token = crate::peers::secret();
        let open = Arc::new(AtomicUsize::new(0));
        for listener in std::iter::once(v4).chain(v6) {
            let (state, stop, token, open) = (state.clone(), stop.clone(), host_token.clone(), open.clone());
            std::thread::spawn(move || accept_loop(listener, state, stop, token, open));
        }
        Ok(HubServer { port, host_token, state, stop })
    }

    /// How many are in the hub (the host included).
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.state.lock().unwrap().members.len()
    }
}

impl Drop for HubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        for m in st.members.drain(..) {
            let _ = m.stream.shutdown(Shutdown::Both);
        }
        drop(st);
        // Wake both accept loops so they see the flag.
        let wake = Duration::from_millis(300);
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), wake);
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv6Addr::LOCALHOST, self.port)), wake);
    }
}

/// Connections open at once, members and ones still saying hello; more are
/// closed straight away (someone with the code opening lots of them).
const MAX_OPEN: usize = MAX_MEMBERS * 2;

fn accept_loop(listener: TcpListener, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>, token: String, open: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let (state, stop, token, open) = (state.clone(), stop.clone(), token.clone(), open.clone());
        std::thread::spawn(move || {
            let _ = serve_member(stream, &state, &stop, &token);
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn serve_member(stream: TcpStream, state: &Mutex<State>, stop: &AtomicBool, token: &str) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(HELLO_WAIT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(hello) = read_line(&mut reader) else { return Ok(()) };
    if hello["op"] != "hello" {
        return Ok(());
    }
    if hello["v"].as_u64().unwrap_or(0) != PROTOCOL {
        send_line(&mut writer, &json!({ "op": "old", "v": PROTOCOL }));
        return Ok(());
    }
    let mut name = clean(hello["name"].as_str().unwrap_or(""), crate::peers::MAX_NAME);
    if name.is_empty() {
        name = "Someone".into();
    }
    let peer = hello["peer"].as_u64().unwrap_or(0);
    let claims_host = hello["host"].as_str().is_some_and(|t| same_secret(t, token));

    let (out_tx, out_rx) = channel::<Value>();
    let id = {
        let mut st = state.lock().unwrap();
        // The same person reconnecting replaces their old entry.
        if peer != 0 {
            if let Some(old) = st.members.iter().find(|m| m.peer == peer).map(|m| m.id) {
                st.remove(old);
            }
        }
        if st.members.len() >= MAX_MEMBERS {
            drop(st);
            send_line(&mut writer, &json!({ "op": "full" }));
            return Ok(());
        }
        let host = claims_host && !st.members.iter().any(|m| m.host);
        let id = st.next_id;
        st.next_id += 1;
        st.members.push(Member {
            id,
            peer,
            name: name.clone(),
            host,
            path: if host { "host" } else { path_of(&hello) },
            rtt: rtt_of(&hello),
            out: out_tx.clone(),
            stream: stream.try_clone()?,
            invites: VecDeque::new(),
        });
        let _ = out_tx.send(json!({ "op": "welcome", "v": PROTOCOL, "you": id, "hub": st.name, "host": host }));
        st.broadcast_members();
        id
    };

    // Everything to this member goes through one writer, in order.
    let writer_thread = std::thread::spawn(move || {
        while let Ok(v) = out_rx.recv() {
            if !send_line(&mut writer, &v) {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });

    stream.set_read_timeout(Some(SILENT_FOR))?;
    while !stop.load(Ordering::Relaxed) {
        let Some(msg) = read_line(&mut reader) else { break };
        match msg["op"].as_str().unwrap_or_default() {
            "ping" => {
                let _ = out_tx.send(json!({ "op": "pong" }));
            }
            "path" => {
                let mut st = state.lock().unwrap();
                let Some(m) = st.members.iter_mut().find(|m| m.id == id) else { break };
                if m.host {
                    continue;
                }
                let (path, rtt) = (path_of(&msg), rtt_of(&msg));
                if m.path != path || m.rtt != rtt {
                    m.path = path;
                    m.rtt = rtt;
                    st.broadcast_members();
                }
            }
            "name" => {
                let name = clean(msg["name"].as_str().unwrap_or(""), crate::peers::MAX_NAME);
                let mut st = state.lock().unwrap();
                if let Some(m) = st.members.iter_mut().find(|m| m.id == id).filter(|m| !name.is_empty() && m.name != name) {
                    m.name = name;
                    st.broadcast_members();
                }
            }
            "invite" => {
                let reply = invite(state, id, &msg);
                let _ = out_tx.send(reply);
            }
            "kick" => {
                let mut st = state.lock().unwrap();
                let is_host = st.members.iter().any(|m| m.id == id && m.host);
                let target = msg["id"].as_u64().filter(|t| *t != id);
                if let (true, Some(t)) = (is_host, target) {
                    if let Some(m) = st.members.iter().find(|m| m.id == t) {
                        let _ = m.out.send(json!({ "op": "kicked" }));
                        // Give the writer a moment to deliver it.
                        let s = m.stream.try_clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(300));
                            if let Ok(s) = s {
                                let _ = s.shutdown(Shutdown::Both);
                            }
                        });
                    }
                    st.members.retain(|m| m.id != t);
                    st.broadcast_members();
                }
            }
            "bye" => break,
            _ => {}
        }
    }
    {
        let mut st = state.lock().unwrap();
        st.remove(id);
    }
    drop(out_tx);
    let _ = writer_thread.join();
    Ok(())
}

fn path_of(msg: &Value) -> &'static str {
    match msg["path"].as_str() {
        Some("direct") => "direct",
        Some("relay") => "relay",
        _ => "unknown",
    }
}

fn rtt_of(msg: &Value) -> Option<u32> {
    msg["rtt"].as_u64().map(|r| r.min(60_000) as u32)
}

/// Passes an invite on to the people it's for. The code must be one the hub
/// can invite to; what it's for comes from the code itself, not from what
/// the sender says it is.
fn invite(state: &Mutex<State>, from: u64, msg: &Value) -> Value {
    let Some((kind, code_clean)) = invitation(msg["code"].as_str().unwrap_or_default()) else {
        return json!({ "op": "invited", "n": 0, "error": "That can't be shared in a hub." });
    };
    let title = clean(msg["title"].as_str().unwrap_or(""), MAX_TITLE);
    let to: Vec<u64> = msg["to"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let mut st = state.lock().unwrap();
    let now = Instant::now();
    let Some(sender) = st.members.iter_mut().find(|m| m.id == from) else {
        return json!({ "op": "invited", "n": 0 });
    };
    while sender.invites.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
        sender.invites.pop_front();
    }
    if sender.invites.len() >= INVITES_PER_MINUTE {
        return json!({ "op": "invited", "n": 0, "error": "That's a lot of invites. Wait a minute and try again." });
    }
    sender.invites.push_back(now);
    let name = sender.name.clone();
    let inv = st.next_inv;
    st.next_inv += 1;
    let out = json!({ "op": "invite", "inv": inv, "from": from, "name": name, "kind": kind, "code": code_clean, "title": title });
    let mut n = 0;
    for m in st.members.iter().filter(|m| m.id != from && (to.is_empty() || to.contains(&m.id))) {
        if m.out.send(out.clone()).is_ok() {
            n += 1;
        }
    }
    json!({ "op": "invited", "n": n })
}

// ------------------------------------------------------------------ client

pub enum HubEvent {
    /// Connected and welcomed: our id, the hub's name, whether we're its host.
    Welcome { you: u64, hub: String, host: bool },
    Members(Vec<Value>),
    Invite(Value),
    /// How many an invite reached, or why none.
    Invited { n: u64, error: String },
    /// Lost the connection; trying again.
    Reconnecting,
    /// The host removed us.
    Kicked,
    Full,
    /// The hub runs a different version of KittyConnect.
    Old,
}

/// Our connection to a hub (ours or a friend's), kept up until dropped.
pub struct HubConn {
    tx: Sender<Value>,
    hello: Arc<Mutex<Value>>,
    stop: Arc<AtomicBool>,
}

impl HubConn {
    pub fn send(&self, v: Value) {
        // The latest path also goes in the hello, so a reconnect shows it at once.
        if v["op"] == "path" || v["op"] == "name" {
            let mut h = self.hello.lock().unwrap();
            for k in ["path", "rtt", "name"] {
                if let Some(x) = v.get(k) {
                    h[k] = x.clone();
                }
            }
        }
        let _ = self.tx.send(v);
    }
}

impl Drop for HubConn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(json!({ "op": "bye" }));
    }
}

/// Connects to the hub at 127.0.0.1:`port` (its server, or the near end of
/// the tunnel to it) and keeps the connection up, reconnecting if it drops.
/// `hello` is sent each time (name, path, and the host secret for our own hub).
/// What it hears goes to `emit`.
pub fn connect(port: u16, hello: Value, emit: impl Fn(HubEvent) + Send + 'static) -> HubConn {
    let (tx, rx) = channel::<Value>();
    let stop = Arc::new(AtomicBool::new(false));
    let hello = Arc::new(Mutex::new(hello));
    let (flag, h) = (stop.clone(), hello.clone());
    std::thread::spawn(move || client_loop(port, h, rx, flag, &emit));
    HubConn { tx, hello, stop }
}

/// Our hub connection's events, to the app's event loop.
fn to_app(gen: u64, proxy: &EventLoopProxy<UserEvent>) -> impl Fn(HubEvent) + Send + 'static {
    let proxy = proxy.clone();
    move |ev| {
        let _ = proxy.send_event(UserEvent::Hub(gen, ev));
    }
}

fn client_loop(port: u16, hello: Arc<Mutex<Value>>, rx: Receiver<Value>, stop: Arc<AtomicBool>, emit: &dyn Fn(HubEvent)) {
    let rx = Arc::new(Mutex::new(rx));
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !first {
            emit(HubEvent::Reconnecting);
            std::thread::sleep(Duration::from_secs(2));
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        first = false;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else { continue };
        stream.set_nodelay(true).ok();
        let hello = hello.lock().unwrap().clone();
        match session(stream, &hello, &rx, &stop, emit) {
            End::Lost => continue,
            End::Stopped => break,
            End::Final(ev) => {
                emit(ev);
                break;
            }
        }
    }
}

enum End {
    /// The connection dropped: try again.
    Lost,
    /// We left.
    Stopped,
    /// The hub won't have us (removed, full, another version).
    Final(HubEvent),
}

fn session(stream: TcpStream, hello: &Value, rx: &Arc<Mutex<Receiver<Value>>>, stop: &AtomicBool, emit: &dyn Fn(HubEvent)) -> End {
    let (Ok(mut writer), Ok(read_half)) = (stream.try_clone(), stream.try_clone()) else { return End::Lost };
    let mut reader = BufReader::new(read_half);
    if !send_line(&mut writer, hello) {
        return End::Lost;
    }
    stream.set_read_timeout(Some(HELLO_WAIT)).ok();
    let Some(welcome) = read_line(&mut reader) else { return End::Lost };
    match welcome["op"].as_str() {
        Some("welcome") => emit(HubEvent::Welcome {
            you: welcome["you"].as_u64().unwrap_or(0),
            hub: clean(welcome["hub"].as_str().unwrap_or(""), 48),
            host: welcome["host"] == true,
        }),
        Some("full") => return End::Final(HubEvent::Full),
        Some("old") => return End::Final(HubEvent::Old),
        _ => return End::Lost,
    }
    stream.set_read_timeout(Some(SILENT_FOR)).ok();

    // Our messages and pings go out from here (anything queued while we were
    // reconnecting goes first); the hub's come in below.
    let done = Arc::new(AtomicBool::new(false));
    let (rx2, done2) = (rx.clone(), done.clone());
    let sender = std::thread::spawn(move || {
        let mut last_ping = Instant::now();
        let mut said_bye = false;
        while !done2.load(Ordering::Relaxed) {
            let next = rx2.lock().ok().and_then(|q| q.recv_timeout(Duration::from_millis(500)).ok());
            if let Some(v) = next {
                said_bye = v["op"] == "bye";
                if !send_line(&mut writer, &v) || said_bye {
                    break;
                }
            }
            if last_ping.elapsed() >= PING_EVERY {
                last_ping = Instant::now();
                if !send_line(&mut writer, &json!({ "op": "ping" })) {
                    break;
                }
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
        said_bye
    });

    let mut end = End::Lost;
    while let Some(msg) = read_line(&mut reader) {
        match msg["op"].as_str().unwrap_or_default() {
            "members" => {
                let list = msg["list"].as_array().cloned().unwrap_or_default();
                emit(HubEvent::Members(list.into_iter().take(MAX_MEMBERS).collect()));
            }
            "invite" => emit(HubEvent::Invite(msg)),
            "invited" => emit(HubEvent::Invited {
                n: msg["n"].as_u64().unwrap_or(0),
                error: clean(msg["error"].as_str().unwrap_or(""), 120),
            }),
            "kicked" => {
                end = End::Final(HubEvent::Kicked);
                break;
            }
            _ => {}
        }
    }
    done.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
    let said_bye = sender.join().unwrap_or(false);
    match end {
        End::Final(_) => end,
        _ if said_bye || stop.load(Ordering::Relaxed) => End::Stopped,
        _ => End::Lost,
    }
}

// -------------------------------------------------------------- in the app

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Hosting,
    Joined,
}

/// An invite we got, until it's joined or dismissed.
struct Invite {
    id: u64,
    from: String,
    kind: &'static str,
    code: String,
    title: String,
    at: Instant,
}

/// The most invites kept; older ones go.
const MAX_INVITES: usize = 20;

/// The Hub tab: the hub we host or joined (one at a time), who's in it, and
/// invites we got.
pub struct Hub {
    gen: u64,
    mode: Mode,
    /// "off" | "starting" | "live" | "ended"
    st: &'static str,
    pub error: String,
    notice: String,
    name: String,
    server: Option<HubServer>,
    proc: Option<Proc>,
    conn: Option<HubConn>,
    code: Option<String>,
    me: u64,
    members: Vec<Value>,
    invites: Vec<Invite>,
    next_invite: u64,
    pub link: Link,
    log: Log,
    /// The path we last told the hub about.
    reported: &'static str,
    /// What the last invite we sent came to.
    sent: String,
    reconnecting: bool,
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            gen: 0,
            mode: Mode::Off,
            st: "off",
            error: String::new(),
            notice: String::new(),
            name: String::new(),
            server: None,
            proc: None,
            conn: None,
            code: None,
            me: 0,
            members: Vec::new(),
            invites: Vec::new(),
            next_invite: 1,
            link: Link::default(),
            log: Log::default(),
            reported: "unknown",
            sent: String::new(),
            reconnecting: false,
        }
    }
}

/// What the page needs to know to join an invite.
pub struct Accepted {
    pub kind: &'static str,
    pub code: String,
}

impl Hub {
    pub fn is_live(&self) -> bool {
        self.st == "live"
    }

    pub fn is_on(&self) -> bool {
        self.mode != Mode::Off
    }

    pub fn is_host(&self) -> bool {
        self.mode == Mode::Hosting
    }

    fn reset(&mut self) {
        self.conn = None; // says bye
        self.proc = None;
        self.server = None;
        self.mode = Mode::Off;
        self.code = None;
        self.me = 0;
        self.members.clear();
        self.link = Link::default();
        self.reported = "unknown";
        self.sent.clear();
        self.reconnecting = false;
        self.gen += 1;
    }

    fn begin(&mut self) {
        self.reset();
        self.error.clear();
        self.notice.clear();
        self.log = Log::default();
    }

    /// Starts a hub on this PC and joins it.
    pub fn host(&mut self, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.begin();
        let me = clean(my_name, crate::peers::MAX_NAME);
        if me.is_empty() {
            return self.error = "Pick your name first (top right).".into();
        }
        let server = match HubServer::start(&format!("{me}'s hub")) {
            Ok(s) => s,
            Err(e) => return self.error = e,
        };
        // The official TailCat, serving the hub's one port and nothing else.
        let args = vec!["serve".into(), "--key=new".into(), server.port.to_string()];
        match tailcat::spawn(Role::HubHost, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        let hello = json!({ "op": "hello", "v": PROTOCOL, "peer": crate::peers::new_id(), "name": me, "host": server.host_token });
        self.conn = Some(connect(server.port, hello, to_app(self.gen, proxy)));
        self.name = format!("{me}'s hub");
        self.server = Some(server);
        self.mode = Mode::Hosting;
        self.st = "starting";
    }

    /// Joins a friend's hub through the official TailCat.
    pub fn join(&mut self, input: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.begin();
        let me = clean(my_name, crate::peers::MAX_NAME);
        if me.is_empty() {
            return self.error = "Pick your name first (top right).".into();
        }
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::Hub => c,
            Ok(_) => return self.error = "That isn't a hub code. Ask for the code from the Hub tab (it starts with kitty:hub:).".into(),
            Err(e) => return self.error = e,
        };
        let local = match crate::free_port() {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        match tailcat::spawn(Role::HubJoin, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        let hello = json!({ "op": "hello", "v": PROTOCOL, "peer": crate::peers::new_id(), "name": me, "path": "unknown" });
        self.conn = Some(connect(local, hello, to_app(self.gen, proxy)));
        self.code = Some(code::hub(c.ports[0], &c.addr));
        self.mode = Mode::Joined;
        self.st = "starting";
    }

    /// Leaves the hub, or closes it if it's ours (everyone in it is disconnected).
    pub fn leave(&mut self) {
        self.reset();
        self.st = "off";
        self.error.clear();
        self.notice.clear();
    }

    fn end(&mut self, why: &str) {
        self.reset();
        self.st = "ended";
        self.notice = why.to_string();
    }

    pub fn rename(&mut self, name: &str) {
        if let Some(c) = &self.conn {
            c.send(json!({ "op": "name", "name": name }));
        }
    }

    /// Invites people in the hub (`to` empty: everyone) to what `code` opens.
    pub fn invite(&mut self, code_text: &str, title: &str, to: Vec<u64>) -> Result<(), String> {
        let Some(c) = self.conn.as_ref().filter(|_| self.is_live()) else {
            return Err("Join or start a hub first.".into());
        };
        if invitation(code_text).is_none() {
            return Err("That can't be shared in a hub.".into());
        }
        self.sent = "Sending…".into();
        c.send(json!({ "op": "invite", "to": to, "code": code_text, "title": clean(title, MAX_TITLE) }));
        Ok(())
    }

    /// Host only: takes someone out of the hub.
    pub fn kick(&self, id: u64) {
        if let (Some(c), true) = (&self.conn, self.is_host()) {
            c.send(json!({ "op": "kick", "id": id }));
        }
    }

    /// Takes an invite off the list to join it.
    pub fn accept(&mut self, id: u64) -> Option<Accepted> {
        let i = self.invites.iter().position(|v| v.id == id)?;
        let inv = self.invites.remove(i);
        Some(Accepted { kind: inv.kind, code: inv.code })
    }

    pub fn dismiss(&mut self, id: u64) {
        self.invites.retain(|v| v.id != id);
    }

    pub fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
        if self.proc.as_ref().map(|p| p.id) != Some(id) || !matches!(role, Role::HubHost | Role::HubJoin) {
            return false;
        }
        match ev {
            TcEvent::Line(line) => {
                if role == Role::HubHost && !tailcat::is_verbose(&line) {
                    if let (Some(addr), Some(srv)) = (tailcat::find_addr(&line), &self.server) {
                        self.code = Some(code::hub(srv.port, &addr));
                        // Our own connection to it is already in (it's on this PC).
                        if self.me != 0 {
                            self.st = "live";
                        }
                    }
                }
                self.link.observe(&line);
                if !tailcat::is_verbose(&line) || tailcat::keep_verbose(&line) {
                    self.log.push(line);
                }
            }
            TcEvent::Exited => {
                let why = crate::last_words(&self.log);
                self.end(&format!("TailCat stopped. {why}"));
            }
        }
        true
    }

    /// Takes in what the hub said. Returns a line for the status bar, if any.
    pub fn on_event(&mut self, gen: u64, ev: HubEvent) -> Option<(String, &'static str)> {
        if gen != self.gen {
            return None;
        }
        match ev {
            HubEvent::Welcome { you, hub, host } => {
                self.me = you;
                if !hub.is_empty() {
                    self.name = hub;
                }
                // Our own hub is live once TailCat has given it a code.
                let was = self.st;
                self.reconnecting = false;
                if self.mode == Mode::Joined || self.code.is_some() {
                    self.st = "live";
                }
                if was != "live" && self.st == "live" {
                    return Some((
                        if host { "Your hub is open. Send its code to your friends.".into() } else { format!("Joined {}.", self.name) },
                        "ok",
                    ));
                }
            }
            HubEvent::Members(list) => self.members = list,
            HubEvent::Invite(v) => {
                // Checked again here: whatever comes in, only codes we can
                // join, written out clean.
                let Some((kind, code_text)) = invitation(v["code"].as_str().unwrap_or_default()) else { return None };
                if v["from"].as_u64() == Some(self.me) {
                    return None;
                }
                let from = clean(v["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME);
                let title = clean(v["title"].as_str().unwrap_or(""), MAX_TITLE);
                // The same invite again (they pressed twice) replaces the old one.
                self.invites.retain(|i| i.code != code_text);
                let id = self.next_invite;
                self.next_invite += 1;
                self.invites.insert(0, Invite { id, from: from.clone(), kind, code: code_text, title, at: Instant::now() });
                self.invites.truncate(MAX_INVITES);
                return Some((format!("{from} invited you to {}. Open the Hub tab to join.", kind_name(kind)), "ok"));
            }
            HubEvent::Invited { n, error } => {
                self.sent = if !error.is_empty() {
                    error
                } else if n == 0 {
                    "Nobody else is in the hub to invite.".into()
                } else {
                    format!("Invited {n} {}.", if n == 1 { "person" } else { "people" })
                };
            }
            HubEvent::Reconnecting => {
                if self.st == "live" {
                    self.reconnecting = true;
                }
            }
            HubEvent::Kicked => {
                self.end("The host removed you from the hub.");
                return Some(("Removed from the hub.".into(), "bad"));
            }
            HubEvent::Full => self.end("That hub is full."),
            HubEvent::Old => self.end("That hub runs a different version of KittyConnect. Update both to the latest."),
        }
        None
    }

    /// Once a second: when our path to the host changes, the hub hears.
    pub fn tick(&mut self) -> bool {
        if self.mode != Mode::Joined || !self.is_live() {
            return false;
        }
        let path = self.link.path();
        if path != self.reported {
            self.reported = path;
            if let Some(c) = &self.conn {
                c.send(json!({ "op": "path", "path": path, "rtt": self.link.rtt_ms }));
            }
        }
        false
    }

    /// For the Connections list: how many others are in the hub.
    pub fn others(&self) -> Vec<&Value> {
        self.members.iter().filter(|m| m["id"].as_u64() != Some(self.me)).collect()
    }

    pub fn title(&self) -> &str {
        &self.name
    }

    pub fn json(&self) -> Value {
        json!({
            "st": self.st,
            "mode": match self.mode { Mode::Off => "off", Mode::Hosting => "host", Mode::Joined => "join" },
            "error": self.error,
            "notice": self.notice,
            "name": self.name,
            "code": self.code,
            "me": self.me,
            "members": self.members,
            "invites": self.invites.iter().map(|i| json!({
                "id": i.id, "from": i.from, "kind": i.kind, "code": i.code, "title": i.title,
                "ago": i.at.elapsed().as_secs(),
            })).collect::<Vec<_>>(),
            "sent": self.sent,
            "reconnecting": self.reconnecting,
            "link": self.link.json(),
            "log": self.log.json(),
        })
    }
}

/// "a voice room", "a file share"… for messages.
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        "voip" => "a voice room",
        "file" => "a file share",
        "game" => "a game server",
        "mount" => "a shared folder",
        _ => "some ports",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw client for the tests: hello, then lines.
    struct Raw {
        w: TcpStream,
        r: BufReader<TcpStream>,
    }

    impl Raw {
        fn join(port: u16, hello: Value) -> Raw {
            let s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut raw = Raw { w: s.try_clone().unwrap(), r: BufReader::new(s) };
            raw.send(hello);
            raw
        }
        fn send(&mut self, v: Value) {
            assert!(send_line(&mut self.w, &v));
        }
        fn next(&mut self) -> Value {
            read_line(&mut self.r).unwrap_or(Value::Null)
        }
        /// The next message with this op, skipping others.
        fn wait(&mut self, op: &str) -> Value {
            for _ in 0..50 {
                let v = self.next();
                if v.is_null() || v["op"] == op {
                    return v;
                }
            }
            Value::Null
        }
    }

    fn hello(name: &str, peer: u64) -> Value {
        json!({ "op": "hello", "v": PROTOCOL, "peer": peer, "name": name, "path": "direct", "rtt": 20 })
    }

    const VOIP: &str = "kitty:voip:50000:tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN";

    #[test]
    fn members_see_each_other_and_invites_reach_only_whom_they_are_for() {
        let hub = HubServer::start("Kit's hub").unwrap();
        let mut host = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        let w = host.wait("welcome");
        assert_eq!((w["you"].as_u64(), w["host"].as_bool(), w["hub"].as_str()), (Some(1), Some(true), Some("Kit's hub")));
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        assert_eq!(jo.wait("welcome")["host"], false);
        let mut sam = Raw::join(hub.port, hello("Sam", 3));
        sam.wait("welcome");
        // Everyone gets the full list, with how each is connected.
        let list = sam.wait("members");
        let list = list["list"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["path"], "host");
        assert_eq!(list[1]["path"], "direct");
        assert_eq!(hub.count(), 3);

        // An invite to Jo alone: Jo gets it, Sam doesn't.
        host.send(json!({ "op": "invite", "to": [2], "code": format!("`{VOIP}`"), "title": "Kit's room", "kind": "file" }));
        let inv = jo.wait("invite");
        assert_eq!(inv["name"], "Kit");
        // The kind comes from the code, not from what the sender said.
        assert_eq!(inv["kind"], "voip");
        assert_eq!(inv["code"], VOIP);
        assert_eq!(host.wait("invited")["n"], 1);
        // Everyone else, from Sam.
        sam.send(json!({ "op": "invite", "to": [], "code": VOIP, "title": "x" }));
        assert_eq!(sam.wait("invited")["n"], 2);
        assert_eq!(jo.wait("invite")["name"], "Sam");

        // Something that isn't a code a hub can invite to goes nowhere.
        jo.send(json!({ "op": "invite", "to": [], "code": "kitty:hub:1:tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN" }));
        assert_eq!(jo.wait("invited")["n"], 0);
        jo.send(json!({ "op": "invite", "to": [], "code": "rm -rf /" }));
        assert_eq!(jo.wait("invited")["n"], 0);
    }

    #[test]
    fn only_the_host_can_remove_people_and_the_secret_is_needed_to_be_host() {
        let hub = HubServer::start("h").unwrap();
        // Claiming to be host with a wrong secret: just a member.
        let mut fake = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 9, "name": "Fake", "host": "0000" }));
        assert_eq!(fake.wait("welcome")["host"], false);
        let mut host = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        assert_eq!(host.wait("welcome")["host"], true);
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        let jo_id = jo.wait("welcome")["you"].as_u64().unwrap();
        // A member can't remove anyone.
        fake.send(json!({ "op": "kick", "id": jo_id }));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hub.count(), 3);
        // The host can.
        host.send(json!({ "op": "kick", "id": jo_id }));
        assert_eq!(jo.wait("kicked")["op"], "kicked");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(hub.count(), 2);
        assert!(read_line(&mut jo.r).is_none(), "the connection is closed");
    }

    #[test]
    fn a_reconnect_replaces_the_old_entry_and_an_old_version_is_told() {
        let hub = HubServer::start("h").unwrap();
        let mut a = Raw::join(hub.port, hello("Jo", 77));
        a.wait("welcome");
        let mut b = Raw::join(hub.port, hello("Jo", 77));
        b.wait("welcome");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hub.count(), 1);
        let mut old = Raw::join(hub.port, json!({ "op": "hello", "v": 999, "name": "Old" }));
        assert_eq!(old.next()["op"], "old");
        // A line too long is the end of that connection.
        let mut big = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "name": "x".repeat(20_000) }));
        assert!(big.next().is_null());
    }

    #[test]
    fn invites_are_rate_limited() {
        let hub = HubServer::start("h").unwrap();
        let mut a = Raw::join(hub.port, hello("A", 1));
        a.wait("welcome");
        let mut b = Raw::join(hub.port, hello("B", 2));
        b.wait("welcome");
        let mut reached = 0;
        for _ in 0..20 {
            a.send(json!({ "op": "invite", "to": [], "code": VOIP }));
            if a.wait("invited")["n"] == 1 {
                reached += 1;
            }
        }
        assert_eq!(reached, INVITES_PER_MINUTE);
    }

    /// The client the app uses, against the server: it's welcomed, sees the
    /// others, gets an invite, comes back after the connection drops, and
    /// stops for good when the host removes it.
    #[test]
    fn the_app_client_end_to_end() {
        use std::sync::mpsc::RecvTimeoutError;
        let hub = HubServer::start("Kit's hub").unwrap();
        let (tx, rx) = channel::<HubEvent>();
        let tx = Mutex::new(tx);
        let conn = connect(hub.port, hello("Jo", 42), move |ev| {
            let _ = tx.lock().unwrap().send(ev);
        });
        let next = |want: &str| -> HubEvent {
            loop {
                let ev = match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(ev) => ev,
                    Err(RecvTimeoutError::Timeout) => panic!("no {want}"),
                    Err(e) => panic!("{e}"),
                };
                let name = match &ev {
                    HubEvent::Welcome { .. } => "welcome",
                    HubEvent::Members(_) => "members",
                    HubEvent::Invite(_) => "invite",
                    HubEvent::Invited { .. } => "invited",
                    HubEvent::Reconnecting => "reconnecting",
                    HubEvent::Kicked => "kicked",
                    HubEvent::Full => "full",
                    HubEvent::Old => "old",
                };
                if name == want {
                    return ev;
                }
            }
        };
        let HubEvent::Welcome { you, host, .. } = next("welcome") else { unreachable!() };
        assert!(!host);
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        loop {
            if let HubEvent::Members(list) = next("members") {
                if list.len() == 2 {
                    break;
                }
            }
        }
        // The path we report shows up for everyone.
        conn.send(json!({ "op": "path", "path": "relay", "rtt": 80 }));
        loop {
            let m = kit.wait("members");
            if m["list"].as_array().unwrap().iter().any(|x| x["name"] == "Jo" && x["path"] == "relay") {
                break;
            }
        }
        kit.send(json!({ "op": "invite", "to": [you], "code": VOIP, "title": "Kit's room" }));
        let HubEvent::Invite(inv) = next("invite") else { unreachable!() };
        assert_eq!((inv["kind"].as_str(), inv["name"].as_str()), (Some("voip"), Some("Kit")));
        // We invite back.
        conn.send(json!({ "op": "invite", "to": [], "code": VOIP, "title": "x" }));
        let HubEvent::Invited { n, .. } = next("invited") else { unreachable!() };
        assert_eq!(n, 1);

        // The connection drops (the tunnel hiccups): it comes back by itself,
        // as the same person, still showing the path it last reported.
        {
            let st = hub.state.lock().unwrap();
            let m = st.members.iter().find(|m| m.name == "Jo").unwrap();
            let _ = m.stream.shutdown(Shutdown::Both);
        }
        next("reconnecting");
        let HubEvent::Welcome { you, .. } = next("welcome") else { unreachable!() };
        std::thread::sleep(Duration::from_millis(200));
        {
            let st = hub.state.lock().unwrap();
            assert_eq!(st.members.iter().filter(|m| m.name == "Jo").count(), 1);
            assert_eq!(st.members.iter().find(|m| m.name == "Jo").unwrap().path, "relay");
        }

        kit.send(json!({ "op": "kick", "id": you }));
        next("kicked");
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(hub.count(), 1, "removed, and it doesn't come back by itself");
        drop(conn);
    }

    #[test]
    fn invite_kinds() {
        assert_eq!(invite_kind(VOIP), Some("voip"));
        let addr = VOIP.rsplit(':').next().unwrap();
        assert_eq!(invite_kind(&format!("kitty:port:25565:{addr}")), Some("port"));
        assert_eq!(invite_kind(&format!("kitty:game.minecraft:25565:{addr}")), Some("game"));
        assert_eq!(invite_kind(addr), None, "a bare address needs port numbers told separately");
        assert_eq!(invite_kind(&format!("kitty:hub:5:{addr}")), None);
        assert_eq!(clean("  a\u{7}b  ", 10), "ab");
        assert!(same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "ab"));
    }
}
