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
//!   ← {"op":"book","book":[{"name":"Kit","code":"kitty:hub:…","at":<secs>}],"gone":["kitty:hub:…"],"goneAt":[{"code":…,"at":…}]}
//!   ← {"op":"offers","list":[{"from":1,"name":"Kit","oid":"voip","kind":"voip","code":"kitty:voip:…","title":"Hangout"}]}
//!   → {"op":"path","path":"relay","rtt":80}
//!   → {"op":"invite","to":[3,4],"code":"kitty:voip:…","title":"Kit's room"}   ([] for everyone)
//!   ← {"op":"invite","inv":7,"from":1,"name":"Kit","kind":"voip","code":"kitty:voip:…","title":"Kit's room"}
//!   ← {"op":"invited","n":2}
//!   → {"op":"address","code":"kitty:hub:…"}       (saved hubs: our own code for it)
//!   → {"op":"merge","book":[…],"gone":[…],"goneAt":[…]}   (saved hubs: our copy of the book;
//!                                                    newest wins, code by code)
//!   → {"op":"forget","code":"kitty:hub:…"}        (we deleted our copy)
//!   → {"op":"offer","oid":"voip","code":"kitty:voip:…","title":"Hangout"}   ("Open to hub")
//!   → {"op":"withdraw","oid":"voip"}
//!   → {"op":"delete"}  ← {"op":"deleted","by":"Sam"}   (delete it for everyone)
//!   ← {"op":"moving","code":"kitty:hub:…","name":"Sam"}  (a saved hub's host closed it:
//!                                    Sam's copy takes over, and everyone goes there)
//!   → {"op":"kick","id":4}                                   (host only)
//!   ← {"op":"kicked"}  (then the hub closes the connection)
//!   → {"op":"ping"}  ← {"op":"pong"}                          (every 15 s; timed for the ping shown)
//!   → {"op":"bye"}
//!
//! Scan sends one line instead of a hello, and gets one back:
//!
//!   → {"op":"peek","v":2}  ← {"op":"peeked","v":2,"id":…,"hub":…,"host":"Kit","members":3,"expiresIn":…,
//!                             "book":[…],"gone":[…],"goneAt":[…]}   (a saved hub's book, so a scan catches up)

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
use crate::hubstore::{self, Entry, Gone, Saved, Store};
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
const PING_EVERY: Duration = Duration::from_secs(5);
/// Silence for this long (5 missed pings) means the other end is gone: a
/// PC that restarted or lost its connection says nothing. 0.12.2 and older
/// ping every 15 s, so they still fit.
const SILENT_FOR: Duration = Duration::from_secs(25);
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
    /// Their own code for a saved hub, once they've said (a rename updates it).
    code: Option<String>,
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
    gone: Vec<Gone>,
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

    /// The time for a change made here to `code`: now, or just after the
    /// last change the book has for it, so it always counts as newer (two
    /// changes in the same second are still in order).
    fn stamp(&self, code: &str) -> u64 {
        let last = self.book.iter().filter(|e| e.code == code).map(|e| e.at).chain(self.gone.iter().filter(|g| g.code == code).map(|g| g.at)).max();
        last.map_or(hubstore::now(), |l| hubstore::now().max(l + 1))
    }

    /// The book, with deleted codes both ways: plain (what 0.12.0 reads) and
    /// with their times.
    fn book_msg(&self) -> Value {
        json!({ "op": "book", "book": self.book, "gone": hubstore::gone_codes(&self.gone), "goneAt": self.gone })
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
            .map(|m| json!({ "id": m.id, "name": m.name, "host": m.host, "path": m.path, "rtt": m.rtt, "code": m.code }))
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

    /// The host is closing a saved hub: whoever has been in it longest, of
    /// those who saved it, takes over with their own copy, and everyone hears
    /// where to go. Returns who, if anyone.
    pub fn hand_over(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.meta.expires?;
        let next = st.members.iter().filter(|m| !m.host && m.code.is_some()).min_by_key(|m| m.id)?;
        let (name, code) = (next.name.clone(), next.code.clone()?);
        st.broadcast(&json!({ "op": "moving", "code": code, "name": name }));
        Some(name)
    }

    /// Another copy of this saved hub is open too, and it's where everyone
    /// should be: everyone here hears to go there (`name`'s, at `code`).
    pub fn send_moving(&self, code: &str, name: &str) {
        let st = self.state.lock().unwrap();
        st.broadcast(&json!({ "op": "moving", "code": code, "name": name }));
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
        let mut reply = json!({
            "op": "peeked", "v": PROTOCOL, "id": st.meta.id, "hub": st.meta.name, "host": host,
            "members": st.members.len(), "expiresIn": st.expires_in(),
        });
        // A saved hub's book goes with it, so whoever scans catches up.
        if st.meta.expires.is_some() {
            let b = st.book_msg();
            for k in ["book", "gone", "goneAt"] {
                reply[k] = b[k].clone();
            }
        }
        // Another copy of this saved hub is open too, and looking at us: our
        // host's app hears, so it looks back (it checks for itself; this is
        // only a hint).
        if let (true, Some(code)) = (st.meta.expires.is_some(), hubstore::clean_code(hello["openAt"].as_str().unwrap_or_default())) {
            if let Some(h) = st.members.iter().find(|m| m.host) {
                let _ = h.out.send(json!({ "op": "alsoOpen", "code": code }));
            }
        }
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
            code: None,
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
                    m.name = name.clone();
                    let code = m.code.clone();
                    st.broadcast_members();
                    // A saved hub's book has their new name, as of now.
                    if let (Some(code), true) = (code, st.meta.expires.is_some()) {
                        let st = &mut *st;
                        let at = st.stamp(&code);
                        if let Some(e) = hubstore::entry(&name, &code, at) {
                            if hubstore::merge(&mut st.book, &mut st.gone, &[e], &[]) {
                                st.broadcast(&st.book_msg());
                            }
                        }
                    }
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
                    // Their code, as of now (saving again after a delete brings it back).
                    Some("address") => {
                        let name = st.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                        let code = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()).unwrap_or_default();
                        let at = st.stamp(&code);
                        match hubstore::entry(&name, &code, at) {
                            Some(e) => {
                                // Everyone's member list says who could take over.
                                let new_code = st.members.iter_mut().find(|m| m.id == id).filter(|m| m.code.as_ref() != Some(&e.code)).map(|m| {
                                    m.code = Some(e.code.clone());
                                });
                                if new_code.is_some() {
                                    st.broadcast_members();
                                }
                                hubstore::merge(&mut st.book, &mut st.gone, &[e], &[])
                            }
                            None => false,
                        }
                    }
                    // Their copy of the book: newest wins, code by code.
                    Some("merge") => {
                        let book: Vec<Entry> = serde_json::from_value(msg["book"].clone()).unwrap_or_default();
                        let gone = hubstore::gone_from(&msg);
                        hubstore::merge(&mut st.book, &mut st.gone, &book, &gone)
                    }
                    // They deleted their copy.
                    _ => {
                        let code = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()).unwrap_or_default();
                        let at = st.stamp(&code);
                        hubstore::merge(&mut st.book, &mut st.gone, &[], &[Gone { code, at }])
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
    Book { book: Vec<Entry>, gone: Vec<Gone> },
    /// What everyone has open in the hub ("Open to hub").
    Offers(Vec<Value>),
    /// Someone deleted the hub for everyone.
    Deleted { by: String },
    /// The host closed the saved hub: go to this code, `name`'s copy.
    Moving { code: String, name: String },
    /// Our saved hub is open at `code` too (that copy just looked at ours).
    AlsoOpen { code: String },
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
            "book" => {
                let (book, gone) = read_book(&msg);
                emit(HubEvent::Book { book, gone });
            }
            "offers" => emit(HubEvent::Offers(msg["list"].as_array().cloned().unwrap_or_default())),
            "deleted" => emit(HubEvent::Deleted { by: clean(msg["by"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) }),
            "moving" => {
                if let Some(code) = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()) {
                    emit(HubEvent::Moving { code, name: clean(msg["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) });
                }
            }
            "alsoOpen" => {
                if let Some(code) = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()) {
                    emit(HubEvent::AlsoOpen { code });
                }
            }
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

/// A book and its deletions as they arrive, checked: hub codes only, names
/// cleaned, within the limits.
fn read_book(msg: &Value) -> (Vec<Entry>, Vec<Gone>) {
    let book = serde_json::from_value::<Vec<Entry>>(msg["book"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| hubstore::entry(&e.name, &e.code, e.at))
        .take(hubstore::MAX_BOOK)
        .collect();
    let gone = hubstore::gone_from(msg)
        .into_iter()
        .filter_map(|g| Some(Gone { code: hubstore::clean_code(&g.code)?, at: g.at }))
        .take(hubstore::MAX_GONE)
        .collect();
    (book, gone)
}

/// What a scan found at one code: an open copy of a hub, and its book.
#[derive(Clone, Debug)]
pub struct Peeked {
    pub id: String,
    pub hub: String,
    pub host: String,
    pub members: u64,
    pub book: Vec<Entry>,
    pub gone: Vec<Gone>,
}

/// Asks whatever answers at 127.0.0.1:`port` (the near end of a tunnel to
/// someone's copy of a hub) whether it's open, without joining. Waits up to
/// `wait`: the tunnel comes up first, and a copy that isn't open never answers.
#[cfg(test)]
pub fn peek(port: u16, wait: Duration) -> Result<Peeked, String> {
    peek_from(port, wait, None, &AtomicBool::new(false))
}

/// A peek, from a copy of the hub that's open itself (at `mine`) if given:
/// the copy we peek hears so, and looks back (two open copies join up).
///
/// TailCat takes our connection at once and only then reaches for the other
/// end; if that first try fails (their relay connection was still coming
/// up, say) it just closes ours. So we ask again, until `wait` is up, or
/// until `stopped` (then our TailCat is closed, which ends a wait at once).
fn peek_from(port: u16, wait: Duration, mine: Option<&str>, stopped: &AtomicBool) -> Result<Peeked, String> {
    let until = Instant::now() + wait;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut ask = json!({ "op": "peek", "v": PROTOCOL });
    if let Some(code) = mine {
        ask["openAt"] = json!(code);
    }
    let v = loop {
        let left = until.saturating_duration_since(Instant::now());
        let answer = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok().and_then(|stream| {
            let mut writer = stream.try_clone().ok()?;
            stream.set_read_timeout(Some(left.max(Duration::from_secs(1)))).ok();
            if !send_line(&mut writer, &ask) {
                return None;
            }
            read_line(&mut BufReader::new(stream))
        });
        match answer {
            Some(v) => break v,
            None if stopped.load(Ordering::Relaxed) => return Err("stopped".into()),
            None if until.saturating_duration_since(Instant::now()) > Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(500))
            }
            None => return Err("not open".into()),
        }
    };
    if v["op"] != "peeked" {
        return Err("not open".into());
    }
    if v["v"].as_u64() != Some(PROTOCOL) {
        return Err("a different version of KittyConnect".into());
    }
    let (book, gone) = read_book(&v);
    Ok(Peeked {
        book,
        gone,
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
const SCAN_AT_ONCE: usize = 8;
/// How long Scan waits for each copy to answer.
const SCAN_WAIT: Duration = Duration::from_secs(20);
/// While we host a saved hub, how long until we look again for another copy
/// of it that's open too (someone took over while we were gone, or opened
/// theirs not knowing where it was): soon at first, then every minute.
fn look_after(n: u32) -> Duration {
    Duration::from_secs(match n {
        0 => 10,
        1 => 20,
        2 => 40,
        _ => 60,
    })
}
/// When another open copy looks at ours, we look back, but no more often
/// than this.
const LOOK_BACK_EVERY: Duration = Duration::from_secs(30);

/// Two copies of a saved hub are open at once, ours (at `mine`, `ours`
/// people in it) and another (at `code`, `theirs`): does everyone belong in
/// theirs? The one with more people in it wins; with the same, the lower
/// code. Both sides ask it the same way round, so exactly one moves.
fn theirs_wins(mine: &str, ours: u64, code: &str, theirs: u64) -> bool {
    theirs > ours || (theirs == ours && code < mine)
}

/// A scan of a saved hub's address book: who has it open right now.
#[derive(Default)]
struct Scan {
    total: usize,
    checked: usize,
    hits: Vec<Hit>,
    /// Every code looked at (people an open copy told us about get looked at too).
    seen: Vec<String>,
    /// People new to us, from open copies' books.
    learned: usize,
    /// Started by Open: when it's done, open ours, or ask.
    open_after: bool,
    /// Our own look while we host it, for another open copy to join up with
    /// (not shown).
    quiet: bool,
    /// Which scan this is: answers to an older one are dropped.
    no: u64,
    /// Ends the checks still going, once they're no use.
    stop: Arc<Stop>,
}

/// Ends a scan's checks early (we joined or opened a hub, or Cancel): none
/// more start, and the TailCat of each one still going is closed, which ends
/// its wait at once.
#[derive(Default)]
struct Stop {
    stopped: AtomicBool,
    running: Mutex<Vec<Arc<Mutex<Option<Proc>>>>>,
}

impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        for p in self.running.lock().unwrap().drain(..) {
            p.lock().unwrap().take();
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Keeps a check's TailCat where `stop` can close it; None (and it's
    /// closed) if the scan has been stopped already.
    fn hold(&self, p: Proc) -> Option<Arc<Mutex<Option<Proc>>>> {
        let mut running = self.running.lock().unwrap();
        if self.is_stopped() {
            return None;
        }
        let slot = Arc::new(Mutex::new(Some(p)));
        running.push(slot.clone());
        Some(slot)
    }
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
    /// Something to say (we moved to another open copy).
    Said(String, &'static str),
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
    gone: Vec<Gone>,
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
    /// To act on our own when the hub moves or its host vanishes: the event
    /// loop and our name, from the last start or join.
    proxy: Option<EventLoopProxy<UserEvent>>,
    me_name: String,
    /// We lost a saved hub's host (reconnecting since).
    lost_since: Option<Instant>,
    /// Who takes over a saved hub whose host vanished, in order (everyone's
    /// app works out the same list from the last member list).
    failover: Option<(String, Vec<Entry>)>,
    /// Scan a hub again soon (we handed ours over, so someone else has it now).
    rescan: Option<(String, Instant)>,
    /// While we host a saved hub: when we next look for another open copy of
    /// it, and how many looks so far.
    watch: Option<(Instant, u32)>,
    /// When our last such look started.
    last_look: Option<Instant>,
    scan_no: u64,
}

/// How long a saved hub's host can be gone before someone else takes over.
const TAKE_OVER_AFTER: Duration = Duration::from_secs(15);
/// How long we wait for the copy we went to (whoever's next in line) before
/// going to the one after: they notice the host is gone a few seconds apart,
/// and their copy takes a moment to open.
const NEXT_AFTER: Duration = Duration::from_secs(45);

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
            proxy: None,
            me_name: String::new(),
            lost_since: None,
            failover: None,
            rescan: None,
            watch: None,
            last_look: None,
            scan_no: 0,
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
        // A hub we joined hears our bye through the tunnel, so our TailCat
        // stays a moment for it (closed at once, the bye was often lost and
        // we stayed listed there until the silence ran out). Its key is a
        // new one each time, so the next TailCat doesn't clash with it.
        match (self.mode, self.proc.take()) {
            (Mode::Joined, Some(p)) => {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    drop(p);
                });
            }
            (_, p) => drop(p),
        }
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
        self.lost_since = None;
        self.watch = None;
        self.gen += 1;
    }

    /// Before we host or join a hub: whatever scans are still going are no
    /// use any more, so they end.
    fn begin(&mut self) {
        self.reset();
        self.stop_scans(None);
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
        self.proxy = Some(proxy.clone());
        self.me_name = me.to_string();
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
            updated: 0,
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
            updated: 0,
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
        let mine = Entry { name: me, code: saved.code.clone(), at: hubstore::now() };
        hubstore::merge(&mut saved.book, &mut saved.gone, &[mine], &[]);
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
        self.proxy = Some(proxy.clone());
        self.me_name = me.clone();
        self.code = Some(code::hub(c.ports[0], &c.addr));
        self.mode = Mode::Joined;
        self.st = "starting";
    }

    /// Our code and our copy of the address book, to the hub we're in.
    fn share_address(&self) {
        let (Some(c), Some(s)) = (&self.conn, self.saved_id.as_ref().and_then(|id| self.saved.iter().find(|s| s.id == *id))) else { return };
        c.send(json!({ "op": "address", "code": s.code }));
        c.send(json!({ "op": "merge", "book": s.book, "gone": hubstore::gone_codes(&s.gone), "goneAt": s.gone }));
    }

    /// Looks through a saved hub's address book for whoever has it open now
    /// (everyone's code but ours). `open_after`: then open ours if nobody
    /// does, or ask.
    pub fn scan(&mut self, id: &str, open_after: bool, proxy: &EventLoopProxy<UserEvent>) -> Option<AfterScan> {
        let s = self.saved.iter().find(|s| s.id == id)?;
        let targets: Vec<Entry> = s.book.iter().filter(|e| e.code != s.code).cloned().collect();
        // One of ours is still going (a quiet look of our own gives way).
        if self.scans.get(id).is_some_and(|sc| sc.checked < sc.total && !sc.quiet) {
            return None;
        }
        self.prompt = None;
        let empty = targets.is_empty();
        self.start_scan(id, targets, open_after, false, proxy);
        (empty && open_after).then(|| AfterScan::OpenOwn(id.to_string()))
    }

    fn start_scan(&mut self, id: &str, targets: Vec<Entry>, open_after: bool, quiet: bool, proxy: &EventLoopProxy<UserEvent>) {
        // One still going for this hub (a quiet look of ours) gives way.
        self.stop_scans(Some(id));
        self.scan_no += 1;
        let no = self.scan_no;
        let seen = targets.iter().map(|e| e.code.clone()).collect();
        let stop = Arc::new(Stop::default());
        self.scans.insert(id.to_string(), Scan { total: targets.len(), open_after, quiet, no, seen, stop: stop.clone(), ..Scan::default() });
        // Our own looks while we host it say so, so a copy we find looks back.
        let mine = quiet.then(|| self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone())).flatten();
        if quiet {
            self.last_look = Some(Instant::now());
        }
        if !targets.is_empty() {
            look(targets, id.to_string(), no, self.tag, mine, stop, proxy);
        }
    }

    /// Ends the checks still going (of hub `only`, or every hub's): we've
    /// joined or opened a hub, or Cancel was pressed. Their TailCats close,
    /// and answers still on the way are dropped; what was found stays.
    fn stop_scans(&mut self, only: Option<&str>) {
        for (_, sc) in self.scans.iter_mut().filter(|(id, _)| only.is_none_or(|o| o == id.as_str())) {
            if sc.checked < sc.total {
                sc.stop.stop();
                sc.total = sc.checked;
                sc.open_after = false;
                // No scan has number 0: whatever else comes for this one is dropped.
                sc.no = 0;
            }
        }
    }

    /// Another copy of the saved hub we host looked at ours: look at it
    /// (once in a while at most, so two copies don't keep looking at each
    /// other), and if it's where everyone should be, go there.
    fn look_back(&mut self, code: &str) {
        if self.mode != Mode::Hosting || !self.is_live() || self.expires.is_none() {
            return;
        }
        let id = self.id.clone();
        let busy = self.scans.get(&id).is_some_and(|sc| sc.checked < sc.total);
        if busy || self.last_look.is_some_and(|t| t.elapsed() < LOOK_BACK_EVERY) {
            // Not right now: our next look at everyone comes early instead.
            let soon = self.last_look.map_or_else(Instant::now, |t| t + LOOK_BACK_EVERY).max(Instant::now() + Duration::from_secs(5));
            if let Some((at, _)) = self.watch.as_mut().filter(|(at, _)| *at > soon) {
                *at = soon;
            }
            return;
        }
        let Some(proxy) = self.proxy.clone() else { return };
        let Some(s) = self.saved.iter().find(|s| s.id == id) else { return };
        if code == s.code {
            return;
        }
        // Whose it is, if they're in our book; the look finds out anyway.
        let name = s.book.iter().find(|e| e.code == code).map(|e| e.name.clone()).unwrap_or_else(|| "Someone".into());
        let target = Entry { name, code: code.to_string(), at: 0 };
        self.start_scan(&id, vec![target], false, true, &proxy);
    }

    /// While we host a saved hub: is another copy of it open too? Whoever is
    /// in here with us isn't hosting it anywhere else, so only the others
    /// are looked at.
    fn look_for_others(&mut self) {
        let Some(proxy) = self.proxy.clone() else { return };
        let id = self.id.clone();
        let Some(s) = self.saved.iter().find(|s| s.id == id) else { return };
        let here: Vec<&str> = self.members.iter().filter_map(|m| m["code"].as_str()).collect();
        let targets: Vec<Entry> = s.book.iter().filter(|e| e.code != s.code && !here.contains(&e.code.as_str())).cloned().collect();
        if targets.is_empty() || self.scans.get(&id).is_some_and(|sc| sc.checked < sc.total) {
            return;
        }
        self.start_scan(&id, targets, false, true, &proxy);
    }

    /// Two copies of our saved hub are open at once (ours, and `host`'s at
    /// `code` with `theirs` people in it): everyone ends up in one. The one
    /// with more people in it stays (the same: the lower code); if that's
    /// theirs, everyone here goes there, us too. Both sides work it out the
    /// same way, so only one moves.
    fn join_up(&mut self, id: &str, code: &str, host: &str, theirs: u64) -> Option<AfterScan> {
        if self.mode != Mode::Hosting || self.id != id || !self.is_live() {
            return None;
        }
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone())?;
        let ours = self.members.len() as u64;
        if code == mine || theirs == 0 || !theirs_wins(&mine, ours, code, theirs) {
            return None;
        }
        let (Some(proxy), me) = (self.proxy.clone(), self.me_name.clone()) else { return None };
        let (hub, expires, others) = (self.name.clone(), self.expires, ours.saturating_sub(1));
        // Everyone here hears where to go first; our copy stays up a moment
        // so they do.
        if let Some(server) = &self.server {
            server.send_moving(code, host);
        }
        let (server, proc) = (self.server.take(), self.proc.take());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            drop(server);
            drop(proc);
        });
        self.plan_failover();
        self.join(code, &me, &proxy);
        if self.mode == Mode::Joined {
            self.id = id.to_string();
            self.expires = expires;
        }
        let people = if theirs == 1 { "person" } else { "people" };
        let with = match others {
            0 => String::new(),
            1 => ", and so did the one person in yours".into(),
            n => format!(", and so did the {n} people in yours"),
        };
        Some(AfterScan::Said(format!("{host} had {hub} open too ({theirs} {people}): you moved there{with}."), "ok"))
    }

    /// One code's answer to a scan. An open copy's book is merged into ours,
    /// and anyone new in it is looked at too.
    pub fn on_scan(&mut self, id: &str, no: u64, entry: Entry, result: Result<Peeked, String>, proxy: &EventLoopProxy<UserEvent>) -> Option<AfterScan> {
        // An answer to a scan that's been replaced since.
        if self.scans.get(id).is_none_or(|sc| sc.no != no) {
            return None;
        }
        let found = result.ok().filter(|p| p.id == id);
        if let Some(p) = &found {
            let learned = self.learn(id, &p.book, &p.gone).unwrap_or(0);
            let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone()).unwrap_or_default();
            let book: Vec<Entry> = self.saved.iter().find(|s| s.id == id).map(|s| s.book.clone()).unwrap_or_default();
            let sc = self.scans.get_mut(id)?;
            sc.learned += learned;
            let more: Vec<Entry> = book.into_iter().filter(|e| e.code != mine && !sc.seen.contains(&e.code)).collect();
            if !more.is_empty() {
                sc.seen.extend(more.iter().map(|e| e.code.clone()));
                sc.total += more.len();
                let from = sc.quiet.then_some(mine);
                look(more, id.to_string(), no, self.tag, from, sc.stop.clone(), proxy);
            }
        }
        let sc = self.scans.get_mut(id)?;
        sc.checked += 1;
        // Only a copy of this same hub counts. Each one shows the moment
        // it's found, while the rest are still being looked at.
        if let Some(p) = found {
            let host = if p.host.is_empty() { entry.name.clone() } else { p.host };
            sc.hits.retain(|h| h.code != entry.code);
            sc.hits.push(Hit { code: entry.code.clone(), host: host.clone(), members: p.members });
            if sc.quiet {
                return self.join_up(id, &entry.code, &host, p.members);
            }
            // Open: it's already online, so offer to join at once (anyone
            // else found joins the list).
            if sc.open_after {
                self.prompt = Some((id.to_string(), sc.hits.clone()));
            }
        }
        if sc.checked < sc.total || !sc.open_after {
            return None;
        }
        // Open, and every look is done: nobody has it open, so ours opens.
        sc.open_after = false;
        sc.hits.is_empty().then(|| AfterScan::OpenOwn(id.to_string()))
    }

    /// Merges another copy's book into our saved copy of hub `id`, newest
    /// wins, and keeps it. Returns how many people are new to us (None if we
    /// don't have it saved).
    fn learn(&mut self, id: &str, book: &[Entry], gone: &[Gone]) -> Option<usize> {
        let s = self.saved.iter_mut().find(|s| s.id == id)?;
        let before: Vec<String> = s.book.iter().map(|e| e.code.clone()).collect();
        if !hubstore::merge(&mut s.book, &mut s.gone, book, gone) {
            return Some(0);
        }
        s.updated = hubstore::now();
        let _ = self.store.store(s);
        Some(s.book.iter().filter(|e| !before.contains(&e.code) && e.code != s.code).count())
    }

    /// Cancel on "It's already online": the looking stops, and the tile is
    /// as it was.
    pub fn dismiss_prompt(&mut self) {
        if let Some((id, _)) = self.prompt.take() {
            self.stop_scans(Some(&id));
            self.scans.remove(&id);
        }
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
            // Just ours deleted: the others keep it (handed over). For
            // everyone: nobody does.
            self.close(!everyone && by.is_none());
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
    pub fn leave(&mut self) -> bool {
        self.close(true)
    }

    /// Leaves or closes. A saved hub we host is handed over first (if
    /// `hand_over` and someone in it saved it): they keep it open, with
    /// everyone else. True if it was.
    fn close(&mut self, hand_over: bool) -> bool {
        let handed = if hand_over { self.hand_over() } else { None };
        let id = self.id.clone();
        self.reset();
        self.failover = None;
        self.st = "off";
        self.error.clear();
        self.notice.clear();
        let Some(name) = handed else { return false };
        self.notice = format!("{name} has it open now, with everyone who was in it. Scan finds it.");
        // A moment for them to open it, then look for it.
        self.rescan = Some((id, Instant::now() + Duration::from_secs(6)));
        true
    }

    fn hand_over(&mut self) -> Option<String> {
        if self.mode != Mode::Hosting || self.expires.is_none() {
            return None;
        }
        let name = self.server.as_ref()?.hand_over()?;
        // Our copy stays up a moment longer, so everyone hears where to go.
        let (server, proc) = (self.server.take(), self.proc.take());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            drop(server);
            drop(proc);
        });
        Some(name)
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
                self.lost_since = None;
                self.failover = None;
                // Hosting a saved hub: someone may have it open elsewhere
                // too (they took over while we were gone); look soon.
                if self.mode == Mode::Hosting && self.expires.is_some() {
                    self.watch = Some((Instant::now() + look_after(0), 0));
                }
                // A saved hub we joined is saved here too, by itself: our own
                // key and code for it, so we can open it ourselves.
                let auto_save = self.mode == Mode::Joined && self.expires.is_some() && self.saved_id.is_none() && self.making.is_none();
                if auto_save {
                    if let Some(p) = self.proxy.clone() {
                        self.save_joined(&p);
                    }
                }
                if was != "live" && self.st == "live" {
                    let msg = match (host, auto_save) {
                        (true, _) => format!("{} is open. Send its code to your friends.", self.name),
                        (false, true) => format!("Joined {}, and saved it here too.", self.name),
                        (false, false) => format!("Joined {}.", self.name),
                    };
                    return Some((msg, "ok"));
                }
            }
            HubEvent::Members(list) => {
                // Everyone else left our saved hub (or vanished): they may
                // be in another copy of it now, so look again soon.
                if self.mode == Mode::Hosting && self.expires.is_some() && list.len() <= 1 && self.members.len() > 1 {
                    self.watch = Some((Instant::now() + look_after(0), 0));
                }
                self.members = list;
            }
            HubEvent::Book { book, gone } => {
                self.book = book;
                self.gone = gone;
                // Our saved copy catches up: whoever came back after a while
                // gets everyone added since, and nothing older wins.
                let new = self.saved_id.clone().and_then(|id| self.learn(&id, &self.book.clone(), &self.gone.clone()));
                // Our "delete for me" reached the hub: finish it.
                if let Some(f) = self.finish.as_ref().filter(|f| !f.everyone) {
                    let id = f.id.clone();
                    let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
                    if mine.is_some_and(|c| self.gone.iter().any(|g| g.code == c)) {
                        return self.finish_delete(&id, None);
                    }
                }
                if let Some(n) = new.filter(|n| *n > 0) {
                    return Some((format!("{}'s address book: {n} new {}.", self.name, if n == 1 { "person" } else { "people" }), "ok"));
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
                // A saved hub's host is gone (or the one we were taking over
                // to never came): someone else takes over before long.
                let taking_over = self.failover.as_ref().is_some_and(|(id, _)| *id == self.id);
                if self.mode == Mode::Joined && self.expires.is_some() && (self.st == "live" || taking_over) && self.lost_since.is_none() {
                    self.lost_since = Some(Instant::now());
                }
            }
            HubEvent::Moving { code, name } => return self.move_to(&code, &name),
            HubEvent::AlsoOpen { code } => self.look_back(&code),
            HubEvent::Kicked => {
                self.end("The host removed you from the hub.");
                return Some(("Removed from the hub.".into(), "bad"));
            }
            HubEvent::Full => self.end("That hub is full."),
            HubEvent::Old => self.end("That hub runs a different version of KittyConnect. Update both to the latest."),
        }
        None
    }

    /// The saved hub moves to `code` (`name`'s copy): ours, so we open it
    /// (everyone comes to us), or theirs, so we go there.
    fn move_to(&mut self, code: &str, name: &str) -> Status {
        let (Some(proxy), me) = (self.proxy.clone(), self.me_name.clone()) else { return None };
        let (id, hub, expires) = (self.id.clone(), self.name.clone(), self.expires);
        // Who's next if that copy never comes (worked out now, while we still
        // have the member list).
        self.plan_failover();
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
        if mine.as_deref() == Some(code) {
            self.open_saved(&id, &me, &proxy);
            return Some((format!("You're hosting {hub} now; everyone is coming over."), "ok"));
        }
        self.join(code, &me, &proxy);
        // Still this hub until that copy answers, so if it never does, we go
        // to the next in line rather than wait for it forever.
        if self.mode == Mode::Joined {
            self.id = id;
            self.expires = expires;
        }
        Some((format!("Moving to {name}'s copy of {hub}…"), "busy"))
    }

    /// Everyone's app works out the same list of who takes over a saved hub
    /// (those in it who saved it, longest in first), from the last member
    /// list, once per hub.
    fn plan_failover(&mut self) {
        if self.failover.as_ref().is_some_and(|(f, _)| *f == self.id) {
            return;
        }
        let mut members: Vec<&Value> = self.members.iter().filter(|m| m["host"] != true).collect();
        members.sort_by_key(|m| m["id"].as_u64().unwrap_or(u64::MAX));
        let list = members
            .into_iter()
            .filter_map(|m| hubstore::entry(m["name"].as_str().unwrap_or(""), m["code"].as_str()?, 0))
            .collect();
        self.failover = Some((self.id.clone(), list));
    }

    /// A saved hub's host vanished: everyone's app takes the same list of who
    /// takes over (those in it who saved it, longest in first) and goes to the
    /// first one; if that one never comes, the next.
    fn take_over(&mut self) -> Status {
        self.plan_failover();
        let lost = self.code.clone().unwrap_or_default();
        let next = self.failover.as_mut().and_then(|(_, l)| {
            while !l.is_empty() {
                let e = l.remove(0);
                if e.code != lost {
                    return Some(e);
                }
            }
            None
        });
        self.lost_since = None;
        match next {
            Some(e) => self.move_to(&e.code, &e.name),
            None => {
                self.failover = None;
                let hub = self.name.clone();
                self.end(&format!("{hub}'s host is gone, and nobody else in it can take over. Open it yourself, or Scan."));
                Some((self.notice.clone(), "bad"))
            }
        }
    }

    /// Once a second: our path and ping to the hub, a delete that's waited
    /// long enough, saved hubs that have expired, and taking over a saved
    /// hub whose host is gone.
    pub fn tick(&mut self) -> Status {
        // The host vanished; or the copy we went to after that never came.
        let limit = if self.failover.as_ref().is_some_and(|(f, _)| *f == self.id) { NEXT_AFTER } else { TAKE_OVER_AFTER };
        if self.lost_since.is_some_and(|t| t.elapsed() >= limit) && self.mode == Mode::Joined {
            return self.take_over();
        }
        if let Some((_, n)) = self.watch.filter(|(at, _)| Instant::now() >= *at) {
            self.watch = Some((Instant::now() + look_after(n + 1), n + 1));
            if self.mode == Mode::Hosting && self.is_live() && self.expires.is_some() {
                self.look_for_others();
            }
        }
        if let Some((id, _)) = self.rescan.as_ref().filter(|(_, at)| Instant::now() >= *at) {
            let id = id.clone();
            self.rescan = None;
            if let (Some(p), false) = (self.proxy.clone(), self.is_on()) {
                self.scan(&id, false, &p);
            }
        }
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
                self.close(false);
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

    /// For the end-to-end check: our copy dies without a word (as if the
    /// app crashed), no hand-over.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_crash(&mut self) {
        self.close(false);
    }

    /// For the end-to-end check: how many of hub `id`'s scan's looks still
    /// have their TailCat running.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_looks_running(&self, id: &str) -> usize {
        self.scans.get(id).map_or(0, |sc| sc.stop.running.lock().unwrap().iter().filter(|p| p.lock().unwrap().is_some()).count())
    }

    /// For the end-to-end check: the PC this copy runs on restarts (or loses
    /// power): TailCat is gone first, so nothing more reaches anyone, not
    /// even a goodbye, and the app with it.
    #[cfg(all(test, target_os = "linux"))]
    pub fn test_power_off(&mut self) {
        if let Some(p) = self.proc.as_mut() {
            p.kill();
        }
        self.close(false);
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
                let scan = self.scans.get(&s.id).filter(|sc| !sc.quiet);
                json!({
                    "id": s.id, "name": s.name, "expiresIn": s.expires.saturating_sub(now),
                    "people": s.book.len(),
                    // When its address book last changed (another copy's had someone new).
                    "updatedAgo": (s.updated > 0).then(|| now.saturating_sub(s.updated)),
                    "here": self.is_on() && self.id == s.id,
                    "scan": scan.map(|sc| json!({
                        "running": sc.checked < sc.total, "checked": sc.checked, "total": sc.total, "learned": sc.learned,
                        // Open is checking whether anyone has it open first.
                        "opening": sc.open_after && sc.checked < sc.total,
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

/// Looks at each code (a few at a time) and reports each answer to the app
/// the moment it comes. `mine`: our code, when we have this hub open
/// ourselves. `stop` ends it early.
fn look(targets: Vec<Entry>, id: String, no: u64, tag: u64, mine: Option<String>, stop: Arc<Stop>, proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        for chunk in targets.chunks(SCAN_AT_ONCE) {
            if stop.is_stopped() {
                return;
            }
            let looks: Vec<_> = chunk
                .iter()
                .cloned()
                .map(|e| {
                    let (p, id, mine, stop) = (proxy.clone(), id.clone(), mine.clone(), stop.clone());
                    std::thread::spawn(move || {
                        let r = scan_one(&e, mine.as_deref(), &stop, &p);
                        let _ = p.send_event(UserEvent::HubScan(tag, id, no, e, r));
                    })
                })
                .collect();
            for l in looks {
                let _ = l.join();
            }
        }
    });
}

/// One look at someone's copy: a TailCat to their code for a moment, and a peek.
fn scan_one(e: &Entry, mine: Option<&str>, stop: &Stop, proxy: &EventLoopProxy<UserEvent>) -> Result<Peeked, String> {
    if stop.is_stopped() {
        return Err("stopped".into());
    }
    let c = code::parse(&e.code)?;
    let local = crate::free_port()?;
    let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
    let Some(slot) = stop.hold(tailcat::spawn(Role::HubScan, &args, proxy)?) else { return Err("stopped".into()) };
    let r = peek_from(local, SCAN_WAIT, mine, &stop.stopped);
    slot.lock().unwrap().take();
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
                    HubEvent::Moving { .. } => "moving",
                    HubEvent::AlsoOpen { .. } => "alsoOpen",
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

    /// Back after weeks with an old book: joining any open copy brings in
    /// everyone added since, and nothing older than what the hub has wins.
    /// A rename updates the book; a peek (Scan) carries it too.
    #[test]
    fn coming_back_after_a_while_catches_up() {
        let (hub, _) = saved("h");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        kit.send(json!({ "op": "address", "code": hub_code(1) }));
        // While we were away: Sam joined and saved it, Jo renamed.
        kit.send(json!({ "op": "merge", "book": [
            { "name": "Sam", "code": hub_code(3), "at": hubstore::now() - 100 },
            { "name": "Jo (new)", "code": hub_code(2), "at": hubstore::now() - 50 },
        ], "goneAt": [] }));
        for _ in 0..3 {
            if kit.wait("book")["book"].as_array().unwrap().len() == 3 {
                break;
            }
        }
        // We come back with our old book: Jo's old name, from weeks ago.
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        let got = jo.wait("book");
        let names: Vec<&str> = got["book"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Sam"), "we get who joined while we were away");
        jo.send(json!({ "op": "merge", "book": [{ "name": "Jo", "code": hub_code(2), "at": hubstore::now() - 14 * 24 * 3600 }], "goneAt": [] }));
        jo.send(json!({ "op": "ping" }));
        jo.wait("pong");
        let names: Vec<String> = hub.state.lock().unwrap().book.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"Jo (new)".to_string()), "an old book doesn't bring an old name back: {names:?}");
        // Jo says their own code, then renames: the book follows.
        jo.send(json!({ "op": "address", "code": hub_code(2) }));
        jo.send(json!({ "op": "name", "name": "Joanna" }));
        for _ in 0..5 {
            let b = kit.wait("book");
            if b["book"].as_array().unwrap().iter().any(|e| e["name"] == "Joanna") {
                break;
            }
        }
        // Scan's peek carries the book, with times, and the deletions both ways.
        let p = peek(hub.port, Duration::from_secs(5)).unwrap();
        assert_eq!(p.book.len(), 3);
        assert!(p.book.iter().all(|e| e.at > 0));
        assert!(p.book.iter().any(|e| e.name == "Joanna"));
    }

    /// Closing a saved hub hands it to whoever has been in it longest, of
    /// those who saved it, and tells everyone where to go.
    #[test]
    fn closing_hands_the_hub_over() {
        let (hub, _) = saved("h");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        kit.send(json!({ "op": "address", "code": hub_code(1) }));
        // Jo came first but never saved it; Sam saved it.
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        let mut sam = Raw::join(hub.port, hello("Sam", 3));
        sam.wait("welcome");
        sam.send(json!({ "op": "address", "code": hub_code(3) }));
        // The member list carries codes (who could take over).
        loop {
            let m = jo.wait("members");
            if m["list"].as_array().unwrap().iter().any(|x| x["name"] == "Sam" && x["code"] == hub_code(3)) {
                break;
            }
        }
        assert_eq!(hub.hand_over().as_deref(), Some("Sam"));
        let mv = jo.wait("moving");
        assert_eq!((mv["code"].as_str(), mv["name"].as_str()), (Some(hub_code(3).as_str()), Some("Sam")));
        // A temporary hub has nobody to hand over to.
        let t = temporary("t");
        let _a = Raw::join(t.port, hello("A", 9));
        assert!(t.hand_over().is_none());
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
    fn a_peek_asks_again_when_the_tunnel_drops_its_first_try() {
        let (hub, id) = saved("Retry Hub");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        // Stands in for TailCat: takes the first connection and closes it
        // (its first reach for the other end failed), then gets through.
        let ln = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let near = ln.local_addr().unwrap().port();
        let far = hub.port;
        std::thread::spawn(move || {
            let (first, _) = ln.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
            let (mut near_side, _) = ln.accept().unwrap();
            let mut far_side = TcpStream::connect((Ipv4Addr::LOCALHOST, far)).unwrap();
            let (mut a, mut b) = (near_side.try_clone().unwrap(), far_side.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut a, &mut b);
            });
            let _ = std::io::copy(&mut far_side, &mut near_side);
        });
        let p = peek(near, Duration::from_secs(10)).expect("the second try answers");
        assert_eq!((p.id.as_str(), p.hub.as_str()), (id.as_str(), "Retry Hub"));
    }

    #[test]
    fn a_peek_from_another_open_copy_reaches_only_the_host() {
        let (hub, _) = saved("h");
        let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
        kit.wait("welcome");
        let mut jo = Raw::join(hub.port, hello("Jo", 2));
        jo.wait("welcome");
        // A plain scan says nothing.
        peek(hub.port, Duration::from_secs(5)).unwrap();
        // A copy that's open itself says where.
        peek_from(hub.port, Duration::from_secs(5), Some(&hub_code(7)), &AtomicBool::new(false)).unwrap();
        let also = kit.wait("alsoOpen");
        assert_eq!(also["code"].as_str(), Some(hub_code(7).as_str()));
        jo.r.get_ref().set_read_timeout(Some(Duration::from_millis(800))).unwrap();
        loop {
            let v = jo.next();
            assert_ne!(v["op"], "alsoOpen", "only the host hears");
            if v.is_null() {
                break;
            }
        }
        // Junk in place of a code: nothing.
        peek_from(hub.port, Duration::from_secs(5), Some("not a code"), &AtomicBool::new(false)).unwrap();
        kit.r.get_ref().set_read_timeout(Some(Duration::from_millis(800))).unwrap();
        loop {
            let v = kit.next();
            assert_ne!(v["op"], "alsoOpen");
            if v.is_null() {
                break;
            }
        }
    }

    #[test]
    fn of_two_open_copies_exactly_one_moves() {
        let (a, b) = (hub_code(1), hub_code(2));
        for (na, nb) in [(1, 1), (1, 2), (2, 1), (3, 3), (5, 2), (2, 9)] {
            let a_moves = theirs_wins(&a, na, &b, nb);
            let b_moves = theirs_wins(&b, nb, &a, na);
            assert!(a_moves != b_moves, "{na} vs {nb}: exactly one moves");
            if na != nb {
                assert_eq!(a_moves, na < nb, "{na} vs {nb}: the one with fewer people moves");
            }
        }
    }

    #[test]
    fn a_stopped_check_ends_at_once() {
        // Stands in for a TailCat that never gets through: takes the
        // connection and says nothing. Stopping closes it (as closing the
        // TailCat would), and the peek ends then, not 20 s later.
        let ln = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let near = ln.local_addr().unwrap().port();
        let stop = Arc::new(Stop::default());
        let s = stop.clone();
        std::thread::spawn(move || {
            let (conn, _) = ln.accept().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            s.stop();
            drop(conn);
        });
        let t = Instant::now();
        let r = peek_from(near, Duration::from_secs(20), None, &stop.stopped);
        assert_eq!(r.err().as_deref(), Some("stopped"));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[test]
    fn stopping_closes_the_checks_tailcats_and_no_more_start() {
        let stop = Stop::default();
        let slot = stop.hold(Proc::stand_in()).expect("held while not stopped");
        assert!(slot.lock().unwrap().is_some());
        stop.stop();
        assert!(slot.lock().unwrap().is_none(), "its TailCat is closed");
        assert!(stop.is_stopped());
        // One that starts after: closed straight away, not held.
        assert!(stop.hold(Proc::stand_in()).is_none());
    }

    #[test]
    fn looks_come_soon_then_every_minute() {
        let secs: Vec<u64> = (0..9).map(|n| look_after(n).as_secs()).collect();
        assert_eq!(secs, [10, 20, 40, 60, 60, 60, 60, 60, 60]);
        // Missing a few pings isn't gone; a restarted PC is, soon.
        assert!(SILENT_FOR >= PING_EVERY * 4 && SILENT_FOR > Duration::from_secs(15) + Duration::from_secs(5));
        assert!(SILENT_FOR + TAKE_OVER_AFTER <= Duration::from_secs(45));
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
