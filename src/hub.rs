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
//! A hub is temporary (a new key every time, like everything else), or
//! saved for up to 13 weeks (see [`crate::hubstore`]): then each person who
//! saves it opens it again with their own same code, and everyone's copies
//! share an address book, so anyone can scan for whoever has it open.
//!
//! One JSON object per line, each way:
//!
//!   → {"op":"hello","v":2,"peer":<random>,"name":"Sam","path":"direct","rtt":20[,"host":<secret>]}
//!   ← {"op":"welcome","v":2,"you":3,"hub":"Minecraft Hub","id":"<32 hex>","expiresIn":<secs>|null,"host":false}
//!     (or {"op":"full"} / {"op":"old"}), then the members, address book and what's open:
//!   ← {"op":"members","list":[{"id":1,"name":"Kit","host":true,"path":"host"},…]}
//!   ← {"op":"book","book":[{"name":"Kit","code":"kitty:hub:…"}],"gone":["kitty:hub:…"]}
//!   ← {"op":"offers","list":[{"from":1,"name":"Kit","oid":"voip","kind":"voip","code":"kitty:voip:…","title":"Hangout"}]}
//!   → {"op":"path","path":"relay","rtt":80}
//!   → {"op":"invite","to":[3,4],"code":"kitty:voip:…","title":"Kit's room"}   ([] for everyone)
//!   ← {"op":"invite","inv":7,"from":1,"name":"Kit","kind":"voip","code":"kitty:voip:…","title":"Kit's room"}
//!   ← {"op":"invited","n":2}
//!   → {"op":"address","code":"kitty:hub:…"}       (saved hubs: our own code for it)
//!   → {"op":"merge","book":[…],"gone":[…]}        (saved hubs: our copy of the book)
//!   → {"op":"forget","code":"kitty:hub:…"}        (we deleted our copy)
//!   → {"op":"offer","oid":"voip","code":"kitty:voip:…","title":"Hangout"}   ("Open to hub")
//!   → {"op":"withdraw","oid":"voip"}
//!   → {"op":"delete"}  ← {"op":"deleted","by":"Sam"}   (delete it for everyone)
//!   → {"op":"kick","id":4}                                   (host only)
//!   ← {"op":"kicked"}  (then the hub closes the connection)
//!   → {"op":"ping"}  ← {"op":"pong"}                          (every 15 s; timed for the ping shown)
//!   → {"op":"bye"}
//!
//! Scan sends one line instead of a hello, and gets one back:
//!
//!   → {"op":"peek","v":2}  ← {"op":"peeked","v":2,"id":…,"hub":…,"host":"Kit","members":3,"expiresIn":…}

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::code::{self, Kind};
use crate::hubstore::{self, Entry, Saved, Store};
use crate::tailcat::{self, Link, Proc, Role, TcEvent};
use crate::{Log, UserEvent};

pub const PROTOCOL: u64 = 2;
/// The most people in one hub.
pub const MAX_MEMBERS: usize = 32;
/// The longest line either side takes (an address book fits).
const MAX_LINE: u64 = 64 * 1024;
/// Things one person can have open in the hub at once ("Open to hub").
const MAX_OFFERS_EACH: usize = 16;
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

/// What a hub is, the same in everyone's copy of a saved one.
#[derive(Clone, Debug)]
pub struct Meta {
    /// 32 hex digits.
    pub id: String,
    pub name: String,
    /// When a saved hub is deleted (seconds since 1970); None for a temporary one.
    pub expires: Option<u64>,
}

/// Something one person has open, shown to everyone ("Open to hub").
struct Offer {
    from: u64,
    oid: String,
    kind: &'static str,
    code: String,
    title: String,
}

struct State {
    meta: Meta,
    members: Vec<Member>,
    next_id: u64,
    next_inv: u64,
    book: Vec<Entry>,
    gone: Vec<String>,
    offers: Vec<Offer>,
}

impl State {
    fn expires_in(&self) -> Option<u64> {
        self.meta.expires.map(|e| e.saturating_sub(hubstore::now()))
    }

    fn broadcast(&self, msg: &Value) {
        for m in &self.members {
            let _ = m.out.send(msg.clone());
        }
    }

    fn book_msg(&self) -> Value {
        json!({ "op": "book", "book": self.book, "gone": self.gone })
    }

    fn offers_msg(&self) -> Value {
        let list: Vec<Value> = self
            .offers
            .iter()
            .map(|o| {
                let name = self.members.iter().find(|m| m.id == o.from).map(|m| m.name.as_str()).unwrap_or("Someone");
                json!({ "from": o.from, "name": name, "oid": o.oid, "kind": o.kind, "code": o.code, "title": o.title })
            })
            .collect();
        json!({ "op": "offers", "list": list })
    }

    fn list(&self) -> Value {
        json!(self
            .members
            .iter()
            .map(|m| json!({ "id": m.id, "name": m.name, "host": m.host, "path": m.path, "rtt": m.rtt }))
            .collect::<Vec<_>>())
    }

    fn broadcast_members(&self) {
        self.broadcast(&json!({ "op": "members", "list": self.list() }));
    }

    /// Takes someone out: their connection closes, what they had open goes,
    /// everyone else hears.
    fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.members.iter().position(|m| m.id == id) else { return false };
        let m = self.members.remove(i);
        let _ = m.stream.shutdown(Shutdown::Both);
        self.broadcast_members();
        let had = self.offers.len();
        self.offers.retain(|o| o.from != id);
        if self.offers.len() != had {
            self.broadcast(&self.offers_msg());
        }
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
    /// Starts a hub's server: on a new free port for a temporary hub, or on
    /// a saved hub's own port (its code names it).
    pub fn start(meta: Meta, port: Option<u16>) -> Result<HubServer, String> {
        let (v4, v6) = match port {
            None => crate::loopback::listen().map_err(|e| format!("Couldn't open the hub: {e}"))?,
            Some(p) => listen_on(p)?,
        };
        let port = v4.local_addr().map_err(|e| e.to_string())?.port();
        let meta = Meta { name: clean(&meta.name, 48), ..meta };
        let state = Arc::new(Mutex::new(State {
            meta,
            members: Vec::new(),
            next_id: 1,
            next_inv: 1,
            book: Vec::new(),
            gone: Vec::new(),
            offers: Vec::new(),
        }));
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

/// A saved hub's own port, on 127.0.0.1 and (where there's IPv6) ::1. If
/// another program has it, the hub can't open under its code.
fn listen_on(port: u16) -> Result<(TcpListener, Option<TcpListener>), String> {
    let busy = || format!("Another program is using port {port}, which this hub's code needs. Close it, or make a new hub.");
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|_| busy())?;
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(l) => Some(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(busy()),
        Err(_) => None,
    };
    Ok((v4, v6))
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
    // Scan: is this hub open, and whose is it? One line back, then done.
    if hello["op"] == "peek" {
        let st = state.lock().unwrap();
        let host = st.members.iter().find(|m| m.host).map(|m| m.name.clone()).unwrap_or_default();
        let reply = json!({
            "op": "peeked", "v": PROTOCOL, "id": st.meta.id, "hub": st.meta.name, "host": host,
            "members": st.members.len(), "expiresIn": st.expires_in(),
        });
        drop(st);
        send_line(&mut writer, &reply);
        return Ok(());
    }
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
        let _ = out_tx.send(json!({
            "op": "welcome", "v": PROTOCOL, "you": id, "hub": st.meta.name, "id": st.meta.id,
            "expiresIn": st.expires_in(), "host": host,
        }));
        st.broadcast_members();
        let _ = out_tx.send(st.book_msg());
        let _ = out_tx.send(st.offers_msg());
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
            // Saved hubs: everyone's codes for it, so anyone can scan for it.
            "address" | "merge" | "forget" => {
                let mut st = state.lock().unwrap();
                if st.meta.expires.is_none() {
                    continue;
                }
                let st = &mut *st;
                let changed = match msg["op"].as_str() {
                    Some("address") => {
                        let name = st.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                        match hubstore::entry(&name, msg["code"].as_str().unwrap_or_default()) {
                            // Saving again brings a deleted code back.
                            Some(e) => {
                                st.gone.retain(|g| *g != e.code);
                                hubstore::merge(&mut st.book, &st.gone, &[e])
                            }
                            None => false,
                        }
                    }
                    Some("merge") => {
                        let gone: Vec<String> =
                            msg["gone"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                        let book: Vec<Entry> = serde_json::from_value(msg["book"].clone()).unwrap_or_default();
                        let a = hubstore::add_gone(&mut st.gone, &gone);
                        let b = hubstore::merge(&mut st.book, &st.gone, &book);
                        a || b
                    }
                    _ => {
                        let code = msg["code"].as_str().unwrap_or_default().to_string();
                        let a = hubstore::add_gone(&mut st.gone, &[code]);
                        let b = hubstore::merge(&mut st.book, &st.gone, &[]);
                        a || b
                    }
                };
                if changed {
                    st.broadcast(&st.book_msg());
                }
            }
            // "Open to hub": something we have open, or not any more.
            "offer" | "withdraw" => {
                let mut st = state.lock().unwrap();
                let oid: String = clean(msg["oid"].as_str().unwrap_or_default(), 64);
                if oid.is_empty() {
                    continue;
                }
                st.offers.retain(|o| !(o.from == id && o.oid == oid));
                if msg["op"] == "offer" {
                    let mine = st.offers.iter().filter(|o| o.from == id).count();
                    if let (Some((kind, code)), true) = (invitation(msg["code"].as_str().unwrap_or_default()), mine < MAX_OFFERS_EACH) {
                        let title = clean(msg["title"].as_str().unwrap_or(""), MAX_TITLE);
                        st.offers.push(Offer { from: id, oid, kind, code, title });
                    }
                }
                st.broadcast(&st.offers_msg());
            }
            // Anyone in it can delete a saved hub for everyone in it now.
            "delete" => {
                let st = state.lock().unwrap();
                let by = st.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                st.broadcast(&json!({ "op": "deleted", "by": by, "id": st.meta.id }));
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
    /// Connected and welcomed: our id, the hub's name, id and time left (a
    /// saved hub), whether we're its host.
    Welcome { you: u64, hub: String, id: String, expires_in: Option<u64>, host: bool },
    /// A saved hub's address book, and the codes deleted from it.
    Book { book: Vec<Entry>, gone: Vec<String> },
    /// What everyone has open in the hub ("Open to hub").
    Offers(Vec<Value>),
    /// Someone deleted the hub for everyone.
    Deleted { by: String },
    /// How long the hub took to answer a ping, in ms.
    Rtt(u32),
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
            id: welcome["id"].as_str().filter(|i| hubstore::is_id(i)).unwrap_or_default().to_string(),
            expires_in: welcome["expiresIn"].as_u64().map(|s| s.min(hubstore::MAX_LIFE)),
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
    // When the last ping went, to time its pong.
    let pinged: Arc<Mutex<Option<Instant>>> = Arc::default();
    let (rx2, done2, pinged2) = (rx.clone(), done.clone(), pinged.clone());
    let sender = std::thread::spawn(move || {
        // The first ping goes soon, so the ping time shows quickly.
        let mut last_ping = Instant::now() - PING_EVERY + Duration::from_secs(2);
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
                *pinged2.lock().unwrap() = Some(last_ping);
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
            "book" => emit(HubEvent::Book {
                book: serde_json::from_value::<Vec<Entry>>(msg["book"].clone())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|e| hubstore::entry(&e.name, &e.code))
                    .take(hubstore::MAX_BOOK)
                    .collect(),
                gone: msg["gone"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).filter_map(|c| hubstore::entry("", c)).map(|e| e.code).collect())
                    .unwrap_or_default(),
            }),
            "offers" => emit(HubEvent::Offers(msg["list"].as_array().cloned().unwrap_or_default())),
            "deleted" => emit(HubEvent::Deleted { by: clean(msg["by"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) }),
            "pong" => {
                if let Some(t) = pinged.lock().unwrap().take() {
                    emit(HubEvent::Rtt(t.elapsed().as_millis().min(60_000) as u32));
                }
            }
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

/// What a scan found at one code: an open copy of a hub.
#[derive(Clone, Debug)]
pub struct Peeked {
    pub id: String,
    pub hub: String,
    pub host: String,
    pub members: u64,
}

/// Asks whatever answers at 127.0.0.1:`port` (the near end of a tunnel to
/// someone's copy of a hub) whether it's open, without joining. Waits up to
/// `wait`: the tunnel comes up first, and a copy that isn't open never answers.
pub fn peek(port: u16, wait: Duration) -> Result<Peeked, String> {
    let until = Instant::now() + wait;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = loop {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(until.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)))).ok();
    if !send_line(&mut writer, &json!({ "op": "peek", "v": PROTOCOL })) {
        return Err("not open".into());
    }
    let v = read_line(&mut BufReader::new(stream)).ok_or("not open")?;
    if v["op"] != "peeked" {
        return Err("not open".into());
    }
    if v["v"].as_u64() != Some(PROTOCOL) {
        return Err("a different version of KittyConnect".into());
    }
    Ok(Peeked {
        id: v["id"].as_str().filter(|i| hubstore::is_id(i)).unwrap_or_default().to_string(),
        hub: clean(v["hub"].as_str().unwrap_or(""), 48),
        host: clean(v["host"].as_str().unwrap_or(""), crate::peers::MAX_NAME),
        members: v["members"].as_u64().unwrap_or(0).min(MAX_MEMBERS as u64),
    })
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
/// Scan checks this many codes at once (each is a TailCat of its own for a moment).
const SCAN_AT_ONCE: usize = 6;
/// How long Scan waits for each copy to answer.
const SCAN_WAIT: Duration = Duration::from_secs(15);

/// A scan of a saved hub's address book: who has it open right now.
#[derive(Default)]
struct Scan {
    total: usize,
    checked: usize,
    hits: Vec<Hit>,
    /// Started by Open: when it's done, open ours, or ask.
    open_after: bool,
}

/// Someone's copy of a saved hub, open right now.
#[derive(Clone, Debug)]
pub struct Hit {
    pub code: String,
    /// Whoever has it open, by their current name.
    pub host: String,
    pub members: u64,
}

/// What the app should do once a scan for Open has finished.
pub enum AfterScan {
    /// Nobody else has it open: open ours.
    OpenOwn(String),
}

/// A delete waiting to hear back from the hub before it finishes.
struct Finish {
    id: String,
    until: Instant,
    everyone: bool,
}

/// The Hub tab: the hub we host or joined (one at a time), who's in it,
/// invites we got, and the hubs saved on this PC.
pub struct Hub {
    /// Tells this app's background results from another's (the end-to-end
    /// check runs two apps in one process).
    pub tag: u64,
    gen: u64,
    mode: Mode,
    /// "off" | "starting" | "live" | "ended"
    st: &'static str,
    pub error: String,
    notice: String,
    name: String,
    /// The hub's id, and when it's deleted if it's a saved one (our clock).
    id: String,
    expires: Option<u64>,
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
    /// The path and ping we last told the hub about.
    reported: &'static str,
    reported_rtt: Option<u32>,
    /// What the last invite we sent came to.
    sent: String,
    reconnecting: bool,
    /// Hubs saved on this PC, and where.
    pub saved: Vec<Saved>,
    pub store: Store,
    /// Which of them this session is (we host it, or joined it and saved it).
    saved_id: Option<String>,
    book: Vec<Entry>,
    gone: Vec<String>,
    /// What others have open in the hub.
    offers: Vec<Value>,
    /// "Open to hub": everything we open is shown to everyone in it.
    open_to_hub: bool,
    /// What we've told the hub we have open: (id, code).
    offered: Vec<(String, String)>,
    /// A key being made for a saved hub: "create" or "save".
    making: Option<&'static str>,
    scans: HashMap<String, Scan>,
    /// Open found the hub already open somewhere: which hub, and where.
    prompt: Option<(String, Vec<Hit>)>,
    finish: Option<Finish>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            tag: crate::peers::new_id(),
            gen: 0,
            mode: Mode::Off,
            st: "off",
            error: String::new(),
            notice: String::new(),
            name: String::new(),
            id: String::new(),
            expires: None,
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
            reported_rtt: None,
            sent: String::new(),
            reconnecting: false,
            saved: Vec::new(),
            store: Store::default(),
            saved_id: None,
            book: Vec::new(),
            gone: Vec::new(),
            offers: Vec::new(),
            open_to_hub: false,
            offered: Vec::new(),
            making: None,
            scans: HashMap::new(),
            prompt: None,
            finish: None,
        }
    }
}

/// What the page needs to know to join an invite.
pub struct Accepted {
    pub kind: &'static str,
    pub code: String,
}

type Status = Option<(String, &'static str)>;

impl Hub {
    /// The Hub tab, with the hubs saved in `store`.
    pub fn with_store(store: Store) -> Hub {
        Hub { saved: store.load_all(), store, ..Hub::default() }
    }

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
        self.reported_rtt = None;
        self.sent.clear();
        self.reconnecting = false;
        self.id.clear();
        self.expires = None;
        self.saved_id = None;
        self.book.clear();
        self.gone.clear();
        self.offers.clear();
        self.offered.clear();
        self.finish = None;
        self.gen += 1;
    }

    fn begin(&mut self) {
        self.reset();
        self.error.clear();
        self.notice.clear();
        self.prompt = None;
        self.log = Log::default();
    }

    /// Starts a temporary hub on this PC (a new key, gone when it closes) and joins it.
    pub fn host_temporary(&mut self, hub_name: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.begin();
        let Some((me, name)) = self.names(hub_name, my_name) else { return };
        let meta = Meta { id: hubstore::new_id(), name, expires: None };
        self.start(meta, None, None, &me, proxy);
    }

    /// Our name and the hub's ("<name>'s hub" when none is given).
    fn names(&mut self, hub_name: &str, my_name: &str) -> Option<(String, String)> {
        let me = clean(my_name, crate::peers::MAX_NAME);
        if me.is_empty() {
            self.error = "Pick your name first (top right).".into();
            return None;
        }
        let name = clean(hub_name, 48);
        Some((me.clone(), if name.is_empty() { format!("{me}'s hub") } else { name }))
    }

    /// The hub's server, served with the official TailCat (its one port and
    /// nothing else): a new key, or a saved hub's own.
    fn start(&mut self, meta: Meta, port: Option<u16>, key: Option<std::path::PathBuf>, me: &str, proxy: &EventLoopProxy<UserEvent>) -> bool {
        let server = match HubServer::start(meta.clone(), port) {
            Ok(s) => s,
            Err(e) => {
                self.error = e;
                return false;
            }
        };
        let key = key.map(|k| format!("--key={}", k.display())).unwrap_or_else(|| "--key=new".into());
        let args = vec!["serve".into(), key, server.port.to_string()];
        match tailcat::spawn(Role::HubHost, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => {
                self.error = e;
                return false;
            }
        }
        let hello = json!({ "op": "hello", "v": PROTOCOL, "peer": crate::peers::new_id(), "name": me, "host": server.host_token });
        self.conn = Some(connect(server.port, hello, to_app(self.gen, proxy)));
        self.name = meta.name;
        self.id = meta.id;
        self.expires = meta.expires;
        self.server = Some(server);
        self.mode = Mode::Hosting;
        self.st = "starting";
        true
    }

    /// Makes a saved hub (its key takes a few seconds), then opens it.
    pub fn create_saved(&mut self, hub_name: &str, secs: u64, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        if self.making.is_some() {
            return;
        }
        self.begin();
        let Some((_, name)) = self.names(hub_name, my_name) else { return };
        let saved = Saved {
            id: hubstore::new_id(),
            name,
            expires: hubstore::now() + hubstore::clamp_life(secs),
            port: 0,
            code: String::new(),
            book: Vec::new(),
            gone: Vec::new(),
        };
        self.make_key(saved, true, proxy, "create");
    }

    /// "Save this hub?": keeps the hub we joined on this PC too, until it
    /// expires for everyone, with our own key and code for it, which goes into
    /// everyone's address book.
    pub fn save_joined(&mut self, proxy: &EventLoopProxy<UserEvent>) {
        let Some(expires) = self.expires.filter(|_| self.mode == Mode::Joined && self.is_live()) else { return };
        if self.making.is_some() || self.saved_id.is_some() || !hubstore::is_id(&self.id) {
            return;
        }
        let left = expires.saturating_sub(hubstore::now());
        if left < 60 {
            return self.error = "This hub is about to expire.".into();
        }
        let saved = Saved {
            id: self.id.clone(),
            name: self.name.clone(),
            expires: hubstore::now() + left.min(hubstore::MAX_LIFE),
            port: 0,
            code: String::new(),
            book: self.book.clone(),
            gone: self.gone.clone(),
        };
        self.make_key(saved, false, proxy, "save");
    }

    /// A port and a key (the official `tailcat genkey`), off the UI thread.
    fn make_key(&mut self, mut saved: Saved, open_after: bool, proxy: &EventLoopProxy<UserEvent>, what: &'static str) {
        saved.port = match hubstore::pick_port() {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let key = match self.store.prepare(&saved.id) {
            Ok(k) => k,
            Err(e) => return self.error = e,
        };
        self.making = Some(what);
        let (proxy, tag) = (proxy.clone(), self.tag);
        std::thread::spawn(move || {
            let result = tailcat::genkey(&key);
            let _ = proxy.send_event(UserEvent::HubKey(tag, saved, result, open_after));
        });
    }

    /// A saved hub's key is made: keep it, then open it (one we made) or
    /// share our code with the hub we're in (one we saved).
    pub fn on_key(&mut self, mut saved: Saved, result: Result<String, String>, open_after: bool, my_name: &str, proxy: &EventLoopProxy<UserEvent>) -> Status {
        self.making = None;
        let addr = match result {
            Ok(a) => a,
            Err(e) => {
                self.store.delete(&saved.id);
                self.error = e;
                return None;
            }
        };
        saved.code = code::hub(saved.port, &addr);
        let me = clean(my_name, crate::peers::MAX_NAME);
        let mine = Entry { name: me, code: saved.code.clone() };
        saved.gone.retain(|g| *g != mine.code);
        hubstore::merge(&mut saved.book, &saved.gone, &[mine]);
        if let Err(e) = self.store.store(&saved) {
            self.store.delete(&saved.id);
            self.error = e;
            return None;
        }
        let (id, name) = (saved.id.clone(), saved.name.clone());
        self.saved.retain(|s| s.id != id);
        self.saved.push(saved);
        self.saved.sort_by_key(|s| s.expires);
        if open_after {
            self.open_saved(&id, my_name, proxy);
            return Some((format!("Opening {name}…"), "busy"));
        }
        if self.mode == Mode::Joined && self.id == id {
            self.saved_id = Some(id);
            self.share_address();
            return Some((format!("Saved {name}. You can open it yourself now, and scan for whoever has it open."), "ok"));
        }
        None
    }

    /// Opens our copy of a saved hub, under our same code as always.
    pub fn open_saved(&mut self, id: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        let Some(s) = self.saved.iter().find(|s| s.id == id).cloned() else { return };
        self.begin();
        let Some((me, _)) = self.names(&s.name, my_name) else { return };
        let Some(key) = self.store.key_path(&s.id) else { return };
        let meta = Meta { id: s.id.clone(), name: s.name.clone(), expires: Some(s.expires) };
        if self.start(meta, Some(s.port), Some(key), &me, proxy) {
            self.saved_id = Some(s.id);
        }
    }

    /// Joins a hub (a friend's, or someone's copy of a saved one) through the
    /// official TailCat.
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

    /// Our code and our copy of the address book, to the hub we're in.
    fn share_address(&self) {
        let (Some(c), Some(s)) = (&self.conn, self.saved_id.as_ref().and_then(|id| self.saved.iter().find(|s| s.id == *id))) else { return };
        c.send(json!({ "op": "address", "code": s.code }));
        c.send(json!({ "op": "merge", "book": s.book, "gone": s.gone }));
    }

    /// Looks through a saved hub's address book for whoever has it open now
    /// (everyone's code but ours). `open_after`: then open ours if nobody
    /// does, or ask.
    pub fn scan(&mut self, id: &str, open_after: bool, proxy: &EventLoopProxy<UserEvent>) -> Option<AfterScan> {
        let s = self.saved.iter().find(|s| s.id == id)?;
        let targets: Vec<Entry> = s.book.iter().filter(|e| e.code != s.code).cloned().collect();
        if self.scans.get(id).is_some_and(|sc| sc.checked < sc.total) {
            return None;
        }
        self.prompt = None;
        self.scans.insert(id.to_string(), Scan { total: targets.len(), open_after, ..Scan::default() });
        if targets.is_empty() {
            return open_after.then(|| AfterScan::OpenOwn(id.to_string()));
        }
        let (proxy, tag, id) = (proxy.clone(), self.tag, id.to_string());
        std::thread::spawn(move || {
            for chunk in targets.chunks(SCAN_AT_ONCE) {
                let looks: Vec<_> = chunk
                    .iter()
                    .cloned()
                    .map(|e| {
                        let (p, id) = (proxy.clone(), id.clone());
                        std::thread::spawn(move || {
                            let r = scan_one(&e, &p);
                            let _ = p.send_event(UserEvent::HubScan(tag, id, e, r));
                        })
                    })
                    .collect();
                for l in looks {
                    let _ = l.join();
                }
            }
        });
        None
    }

    /// One code's answer to a scan.
    pub fn on_scan(&mut self, id: &str, entry: Entry, result: Result<Peeked, String>) -> Option<AfterScan> {
        let sc = self.scans.get_mut(id)?;
        sc.checked += 1;
        // Only a copy of this same hub counts.
        if let Some(p) = result.ok().filter(|p| p.id == id) {
            let host = if p.host.is_empty() { entry.name.clone() } else { p.host };
            sc.hits.retain(|h| h.code != entry.code);
            sc.hits.push(Hit { code: entry.code, host, members: p.members });
        }
        if sc.checked < sc.total || !sc.open_after {
            return None;
        }
        sc.open_after = false;
        if sc.hits.is_empty() {
            return Some(AfterScan::OpenOwn(id.to_string()));
        }
        self.prompt = Some((id.to_string(), sc.hits.clone()));
        None
    }

    pub fn dismiss_prompt(&mut self) {
        self.prompt = None;
    }

    /// Deletes a saved hub: just ours (our code leaves everyone's address
    /// book, if we're in the hub now), or for everyone in it right now
    /// (anyone in it can).
    pub fn delete(&mut self, id: &str, everyone: bool) -> Status {
        let here = self.is_live() && self.id == id;
        if everyone {
            let Some(c) = self.conn.as_ref().filter(|_| here) else {
                self.error = "Open or join the hub first: deleting it for everyone reaches those in it.".into();
                return None;
            };
            c.send(json!({ "op": "delete" }));
            self.finish = Some(Finish { id: id.to_string(), until: Instant::now() + Duration::from_secs(3), everyone: true });
            return Some(("Deleting it for everyone…".into(), "busy"));
        }
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
        if let (true, Some(c), Some(code)) = (here, &self.conn, mine) {
            c.send(json!({ "op": "forget", "code": code }));
            self.finish = Some(Finish { id: id.to_string(), until: Instant::now() + Duration::from_secs(2), everyone: false });
            return Some(("Deleting…".into(), "busy"));
        }
        self.finish_delete(id, None)
    }

    /// Our copy goes, key and all; if we're in it, we leave (or close it).
    fn finish_delete(&mut self, id: &str, by: Option<String>) -> Status {
        let name = self.saved.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_else(|| self.name.clone());
        let everyone = self.finish.as_ref().is_some_and(|f| f.everyone);
        self.store.delete(id);
        self.saved.retain(|s| s.id != id);
        self.scans.remove(id);
        if self.prompt.as_ref().is_some_and(|(p, _)| p == id) {
            self.prompt = None;
        }
        self.finish = None;
        if self.is_on() && self.id == id {
            self.leave();
        }
        let msg = match by {
            Some(by) if !everyone => format!("{by} deleted {name} for everyone."),
            _ if everyone => format!("Deleted {name} for everyone in it."),
            _ => format!("Deleted {name}."),
        };
        self.notice = msg.clone();
        Some((msg, "idle"))
    }

    /// Leaves the hub, or closes it if it's ours (everyone in it is
    /// disconnected; a saved hub stays saved).
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

    /// "Open to hub" on or off.
    pub fn set_open(&mut self, on: bool) {
        self.open_to_hub = on;
    }

    /// What we have open now, as (id, title, code): with "Open to hub" on,
    /// everyone in the hub sees it, and stops seeing each thing once it closes.
    pub fn sync_offers(&mut self, open: Vec<(String, String, String)>) {
        let Some(c) = self.conn.as_ref().filter(|_| self.is_live()) else {
            self.offered.clear();
            return;
        };
        let want = if self.open_to_hub { open } else { Vec::new() };
        for (oid, code) in &self.offered {
            if !want.iter().any(|(o, _, c2)| o == oid && c2 == code) {
                c.send(json!({ "op": "withdraw", "oid": oid }));
            }
        }
        for (oid, title, code) in &want {
            if !self.offered.iter().any(|(o, c2)| o == oid && c2 == code) {
                c.send(json!({ "op": "offer", "oid": oid, "title": title, "code": code }));
            }
        }
        self.offered = want.into_iter().map(|(o, _, c)| (o, c)).collect();
    }

    /// Joins something someone has open in the hub.
    pub fn accept_offer(&self, from: u64, oid: &str) -> Option<Accepted> {
        let o = self.offers.iter().find(|o| o["from"].as_u64() == Some(from) && o["oid"] == oid)?;
        let (kind, code) = invitation(o["code"].as_str()?)?;
        Some(Accepted { kind, code })
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
    pub fn on_event(&mut self, gen: u64, ev: HubEvent) -> Status {
        if gen != self.gen {
            return None;
        }
        match ev {
            HubEvent::Welcome { you, hub, id, expires_in, host } => {
                self.me = you;
                if !hub.is_empty() {
                    self.name = hub;
                }
                if self.mode == Mode::Joined {
                    self.id = id;
                    self.expires = expires_in.map(|s| hubstore::now() + s);
                    // A hub we saved before: our code goes in its book again.
                    if self.expires.is_some() && self.saved.iter().any(|s| s.id == self.id) {
                        self.saved_id = Some(self.id.clone());
                    }
                }
                self.share_address();
                // A reconnect starts the hub's list of what we have open afresh.
                self.offered.clear();
                // Our own hub is live once TailCat has given it a code.
                let was = self.st;
                self.reconnecting = false;
                // The hub answered through the tunnel: it's up, direct or relay.
                if self.mode == Mode::Joined {
                    self.link.peer_seen = true;
                }
                if self.mode == Mode::Joined || self.code.is_some() {
                    self.st = "live";
                }
                if was != "live" && self.st == "live" {
                    return Some((
                        if host { format!("{} is open. Send its code to your friends.", self.name) } else { format!("Joined {}.", self.name) },
                        "ok",
                    ));
                }
            }
            HubEvent::Members(list) => self.members = list,
            HubEvent::Book { book, gone } => {
                self.book = book;
                self.gone = gone;
                if let Some(s) = self.saved_id.as_ref().and_then(|id| self.saved.iter_mut().find(|s| s.id == *id)) {
                    let a = hubstore::add_gone(&mut s.gone, &self.gone);
                    let b = hubstore::merge(&mut s.book, &s.gone, &self.book);
                    if a || b {
                        let _ = self.store.store(s);
                    }
                }
                // Our "delete for me" reached the hub: finish it.
                if let Some(f) = self.finish.as_ref().filter(|f| !f.everyone) {
                    let id = f.id.clone();
                    let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
                    if mine.is_some_and(|c| self.gone.contains(&c)) {
                        return self.finish_delete(&id, None);
                    }
                }
            }
            HubEvent::Offers(list) => {
                // Checked again here: only codes we can join, from others.
                self.offers = list
                    .into_iter()
                    .filter(|o| o["from"].as_u64() != Some(self.me))
                    .filter_map(|o| {
                        let (kind, code) = invitation(o["code"].as_str()?)?;
                        Some(json!({
                            "from": o["from"], "oid": clean(o["oid"].as_str()?, 64), "kind": kind, "code": code,
                            "name": clean(o["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME),
                            "title": clean(o["title"].as_str().unwrap_or(""), MAX_TITLE),
                        }))
                    })
                    .take(MAX_MEMBERS * MAX_OFFERS_EACH)
                    .collect();
            }
            HubEvent::Deleted { by } => {
                let id = self.id.clone();
                return self.finish_delete(&id, Some(by));
            }
            HubEvent::Rtt(ms) => self.link.rtt_ms = Some(ms),
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

    /// Once a second: our path and ping to the hub, a delete that's waited
    /// long enough, and saved hubs that have expired.
    pub fn tick(&mut self) -> Status {
        if let Some(f) = self.finish.as_ref().filter(|f| Instant::now() >= f.until) {
            let id = f.id.clone();
            return self.finish_delete(&id, None);
        }
        let now = hubstore::now();
        if let Some(s) = self.saved.iter().find(|s| s.expires <= now) {
            let (id, name) = (s.id.clone(), s.name.clone());
            self.store.delete(&id);
            self.saved.retain(|s| s.id != id);
            self.scans.remove(&id);
            if self.is_on() && self.id == id {
                self.leave();
            }
            self.notice = format!("{name} expired and was deleted.");
            return Some((self.notice.clone(), "idle"));
        }
        if self.mode == Mode::Joined && self.is_live() {
            let (path, rtt) = (self.link.path(), self.link.rtt_ms);
            let moved = match (self.reported_rtt, rtt) {
                (None, Some(_)) => true,
                (Some(a), Some(b)) => a.abs_diff(b) >= 5.max(a / 7),
                _ => false,
            };
            if path != self.reported || moved {
                self.reported = path;
                self.reported_rtt = rtt;
                if let Some(c) = &self.conn {
                    c.send(json!({ "op": "path", "path": path, "rtt": rtt }));
                }
            }
        }
        None
    }

    /// For the Connections list: how many others are in the hub.
    pub fn others(&self) -> Vec<&Value> {
        self.members.iter().filter(|m| m["id"].as_u64() != Some(self.me)).collect()
    }

    pub fn title(&self) -> &str {
        &self.name
    }

    /// A saved hub (closing it keeps it), rather than a temporary one.
    pub fn is_saved(&self) -> bool {
        self.expires.is_some()
    }

    /// For the end-to-end check: events from two hubs in one process are
    /// told apart by generation.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_gen_base(&mut self, base: u64) {
        self.gen = base;
    }

    pub fn json(&self) -> Value {
        let now = hubstore::now();
        json!({
            "st": self.st,
            "mode": match self.mode { Mode::Off => "off", Mode::Hosting => "host", Mode::Joined => "join" },
            "error": self.error,
            "notice": self.notice,
            "name": self.name,
            "id": self.id,
            "kind": if self.expires.is_some() { "saved" } else { "temporary" },
            "expiresIn": self.expires.map(|e| e.saturating_sub(now)),
            "savedHere": self.saved_id.is_some(),
            "canSave": self.mode == Mode::Joined && self.is_live() && self.expires.is_some() && self.saved_id.is_none(),
            "making": self.making,
            "code": self.code,
            "me": self.me,
            "members": self.members,
            "invites": self.invites.iter().map(|i| json!({
                "id": i.id, "from": i.from, "kind": i.kind, "code": i.code, "title": i.title,
                "ago": i.at.elapsed().as_secs(),
            })).collect::<Vec<_>>(),
            "offers": self.offers,
            "openToHub": self.open_to_hub,
            "sent": self.sent,
            "reconnecting": self.reconnecting,
            "link": self.link.json(),
            "log": self.log.json(),
            "saved": self.saved.iter().map(|s| {
                let scan = self.scans.get(&s.id);
                json!({
                    "id": s.id, "name": s.name, "expiresIn": s.expires.saturating_sub(now),
                    "people": s.book.len(),
                    "here": self.is_on() && self.id == s.id,
                    "scan": scan.map(|sc| json!({
                        "running": sc.checked < sc.total, "checked": sc.checked, "total": sc.total,
                        "hits": sc.hits.iter().map(|h| json!({ "code": h.code, "host": h.host, "members": h.members })).collect::<Vec<_>>(),
                    })),
                })
            }).collect::<Vec<_>>(),
            "prompt": self.prompt.as_ref().map(|(id, hits)| json!({
                "id": id,
                "name": self.saved.iter().find(|s| s.id == *id).map(|s| s.name.clone()).unwrap_or_default(),
                "hits": hits.iter().map(|h| json!({ "code": h.code, "host": h.host, "members": h.members })).collect::<Vec<_>>(),
            })),
        })
    }
}

/// One look at someone's copy: a TailCat to their code for a moment, and a peek.
fn scan_one(e: &Entry, proxy: &EventLoopProxy<UserEvent>) -> Result<Peeked, String> {
    let c = code::parse(&e.code)?;
    let local = crate::free_port()?;
    let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
    let proc = tailcat::spawn(Role::HubScan, &args, proxy)?;
    let r = peek(local, SCAN_WAIT);
    drop(proc);
    r
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

    /// A temporary hub's server.
    fn temporary(name: &str) -> HubServer {
        HubServer::start(Meta { id: hubstore::new_id(), name: name.into(), expires: None }, None).unwrap()
    }

    /// A saved hub's server, on its own port.
    fn saved(name: &str) -> (HubServer, String) {
        let id = hubstore::new_id();
        let port = hubstore::pick_port().unwrap();
        let meta = Meta { id: id.clone(), name: name.into(), expires: Some(hubstore::now() + 7 * 24 * 3600) };
        let hub = HubServer::start(meta, Some(port)).unwrap();
        assert_eq!(hub.port, port);
        (hub, id)
    }

    fn hub_code(n: u8) -> String {
        code::hub(21000 + n as u16, &format!("tc{}", (b'a' + n) as char).repeat(30))
    }

    const VOIP: &str = "kitty:voip:50000:tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN";

    #[test]
    fn members_see_each_other_and_invites_reach_only_whom_they_are_for() {
        let hub = temporary("Kit's hub");
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
        let hub = temporary("h");
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
        let hub = temporary("h");
        let mut a = Raw::join(hub.port, hello("Jo", 77));
        a.wait("welcome");
        let mut b = Raw::join(hub.port, hello("Jo", 77));
        b.wait("welcome");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hub.count(), 1);
        let mut old = Raw::join(hub.port, json!({ "op": "hello", "v": 999, "name": "Old" }));
        assert_eq!(old.next()["op"], "old");
        // A line too long is the end of that connection.
        let mut big = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "name": "x".repeat(70_000) }));
        assert!(big.next().is_null());
    }

    #[test]
    fn invites_are_rate_limited() {
        let hub = temporary("h");
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
        let hub = temporary("Kit's hub");
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
                    HubEvent::Book { .. } => "book",
                    HubEvent::Offers(_) => "offers",
                    HubEvent::Deleted { .. } => "deleted",
                    HubEvent::Rtt(_) => "rtt",
                };
                if name == want {
                    return ev;
                }
            }
        };
        let HubEvent::Welcome { you, host, .. } = next("welcome") else { unreachable!() };
        assert!(!host);
        // The ping shown next to everyone: timed from the hub's answer.
        let HubEvent::Rtt(ms) = next("rtt") else { unreachable!() };
        assert!(ms < 1000);
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

    /// A saved hub's address book: each person's code, merged from everyone's
    /// copies, and a deleted code stays out.
    #[test]
    fn a_saved_hub_shares_everyones_codes() {
        let (hub, id) = saved("Minecraft Hub");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        let w = kit.wait("welcome");
        assert_eq!((w["id"].as_str(), w["hub"].as_str()), (Some(id.as_str()), Some("Minecraft Hub")));
        let left = w["expiresIn"].as_u64().unwrap();
        assert!(left > 7 * 24 * 3600 - 60 && left <= 7 * 24 * 3600, "time left, not a date");
        assert_eq!(kit.wait("book")["book"], json!([]), "a new hub's book starts empty");
        kit.send(json!({ "op": "address", "code": hub_code(1) }));
        assert_eq!(kit.wait("book")["book"][0]["name"], "Kit");

        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        let b = jo.wait("book");
        assert_eq!(b["book"].as_array().unwrap().len(), 1, "a newcomer gets the book");
        // Jo's own code, and Jo's copy of the book (with someone Kit didn't know).
        jo.send(json!({ "op": "address", "code": hub_code(2) }));
        jo.send(json!({ "op": "merge", "book": [{ "name": "Sam", "code": hub_code(3) }, { "name": "Bad", "code": VOIP }], "gone": [] }));
        let mut book = Value::Null;
        for _ in 0..3 {
            book = kit.wait("book");
            if book["book"].as_array().unwrap().len() == 3 {
                break;
            }
        }
        let names: Vec<&str> = book["book"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Kit", "Jo", "Sam"], "not a room code, only hub codes");
        // Sam deleted their copy: their code leaves, and an old book can't bring it back.
        jo.send(json!({ "op": "forget", "code": hub_code(3) }));
        let b = kit.wait("book");
        assert_eq!(b["book"].as_array().unwrap().len(), 2);
        assert_eq!(b["gone"][0], hub_code(3));
        kit.send(json!({ "op": "merge", "book": [{ "name": "Sam", "code": hub_code(3) }], "gone": [] }));
        jo.send(json!({ "op": "ping" }));
        assert_eq!(jo.wait("pong")["op"], "pong");
        assert_eq!(hub.state.lock().unwrap().book.len(), 2);
    }

    #[test]
    fn a_temporary_hub_keeps_no_book() {
        let hub = temporary("t");
        let mut kit = Raw::join(hub.port, hello("Kit", 1));
        assert!(kit.wait("welcome")["expiresIn"].is_null());
        kit.send(json!({ "op": "address", "code": hub_code(1) }));
        kit.send(json!({ "op": "ping" }));
        kit.wait("pong");
        assert!(hub.state.lock().unwrap().book.is_empty());
    }

    /// "Open to hub": what each person has open, shown to everyone, gone
    /// when it closes or they leave.
    #[test]
    fn whats_open_is_shown_until_it_closes() {
        let hub = temporary("t");
        let mut kit = Raw::join(hub.port, hello("Kit", 1));
        kit.wait("welcome");
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        assert_eq!(jo.wait("offers")["list"], json!([]));
        kit.send(json!({ "op": "offer", "oid": "voip", "code": format!("`{VOIP}`"), "title": "Hangout" }));
        let o = jo.wait("offers");
        assert_eq!(o["list"][0]["name"], "Kit");
        assert_eq!(o["list"][0]["kind"], "voip");
        assert_eq!(o["list"][0]["code"], VOIP, "written out clean");
        // Not something a hub can share: refused.
        kit.send(json!({ "op": "offer", "oid": "x", "code": hub_code(1), "title": "x" }));
        assert_eq!(jo.wait("offers")["list"].as_array().unwrap().len(), 1);
        kit.send(json!({ "op": "withdraw", "oid": "voip" }));
        assert_eq!(jo.wait("offers")["list"], json!([]));
        // Leaving takes everything they had open with them.
        kit.send(json!({ "op": "offer", "oid": "voip", "code": VOIP, "title": "Hangout" }));
        assert_eq!(jo.wait("offers")["list"].as_array().unwrap().len(), 1);
        kit.send(json!({ "op": "bye" }));
        assert_eq!(jo.wait("offers")["list"], json!([]));
    }

    /// Anyone in it can delete a hub for everyone in it.
    #[test]
    fn delete_reaches_everyone_in_it() {
        let (hub, _) = saved("h");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        jo.send(json!({ "op": "delete" }));
        assert_eq!(kit.wait("deleted")["by"], "Jo");
        assert_eq!(jo.wait("deleted")["by"], "Jo");
    }

    /// Scan: a peek says what the hub is and whose copy, without joining.
    #[test]
    fn a_peek_answers_without_joining() {
        let (hub, id) = saved("Minecraft Hub");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        let p = peek(hub.port, Duration::from_secs(5)).unwrap();
        assert_eq!((p.id.as_str(), p.hub.as_str(), p.host.as_str(), p.members), (id.as_str(), "Minecraft Hub", "Kit", 1));
        assert_eq!(hub.count(), 1, "peeking isn't joining");
        // Nothing there: no answer.
        let nothing = crate::free_port().unwrap();
        assert!(peek(nothing, Duration::from_millis(800)).is_err());
    }

    #[test]
    fn a_saved_hub_needs_its_own_port() {
        let (hub, _) = saved("h");
        let again = HubServer::start(Meta { id: hubstore::new_id(), name: "x".into(), expires: Some(hubstore::now() + 60) }, Some(hub.port));
        assert!(again.err().unwrap().contains("Another program"));
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
