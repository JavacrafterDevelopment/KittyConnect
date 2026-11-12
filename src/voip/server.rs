//! The room a VOIP host runs: one text channel, one voice channel, and the list
//! of who's here. It listens on 127.0.0.1 only; friends reach it through
//! `tailcat serve`, so every byte between members crosses the encrypted tunnel.
//! The host joins their own room over loopback, which never leaves the PC.
//!
//! Voice is relayed, not mixed: each speaker's frames go to everyone else in
//! voice, and each member mixes what they receive.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::wire::{self, Out};

const HISTORY: usize = 200;
const MAX_NAME: usize = 32;
const MAX_TEXT: usize = 2000;
/// Members ping every 30 seconds; one that's silent this long is gone.
const IDLE: Duration = Duration::from_secs(100);
const QUEUE: usize = 512;

struct Member {
    id: u16,
    name: String,
    host: bool,
    voice: bool,
    muted: bool,
    deaf: bool,
    path: String,
    rtt: Option<u32>,
    tx: SyncSender<Out>,
}

struct Room {
    name: String,
    members: Vec<Member>,
    history: VecDeque<Value>,
    next_id: u16,
    next_msg: u64,
}

pub struct RoomServer {
    pub port: u16,
    /// Lets the host's own connection be marked as the host. It never leaves
    /// this PC: the host connects over loopback.
    pub host_token: String,
    stop: Arc<AtomicBool>,
    room: Arc<Mutex<Room>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..4u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Printable, trimmed, length-capped text from a member.
pub fn clean(s: &str, max: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control() || *c == '\n').collect();
    s.trim().chars().take(max).collect()
}

impl RoomServer {
    pub fn start(name: &str) -> Result<RoomServer, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't open the room: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let name = clean(name, 40);
        let room = Arc::new(Mutex::new(Room {
            name: if name.is_empty() { "Hangout".into() } else { name },
            members: Vec::new(),
            history: VecDeque::new(),
            next_id: 1,
            next_msg: 1,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let host_token = token();
        let (r, s, t) = (room.clone(), stop.clone(), host_token.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (r, t) = (r.clone(), t.clone());
                std::thread::spawn(move || serve_member(stream, &r, &t));
            }
        });
        Ok(RoomServer { port, host_token, stop, room })
    }
}

impl Drop for RoomServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let closed = wire::json(&json!({ "t": "closed", "reason": "The host ended the room." }));
        if let Ok(mut room) = self.room.lock() {
            for m in room.members.drain(..) {
                let _ = m.tx.try_send(Out::Frame(closed.clone()));
                let _ = m.tx.try_send(Out::Close);
            }
        }
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_millis(300));
    }
}

fn send(tx: &SyncSender<Out>, bytes: Vec<u8>) {
    // A member who can't keep up loses frames rather than stalling the room.
    if let Err(TrySendError::Full(_)) = tx.try_send(Out::Frame(bytes)) {}
}

fn members_json(room: &Room) -> Value {
    json!({
        "t": "members",
        "members": room.members.iter().map(|m| json!({
            "id": m.id, "name": m.name, "host": m.host, "voice": m.voice,
            "muted": m.muted, "deaf": m.deaf, "path": m.path, "rtt": m.rtt,
        })).collect::<Vec<_>>(),
    })
}

fn broadcast(room: &Room, v: &Value) {
    let bytes = wire::json(v);
    for m in &room.members {
        send(&m.tx, bytes.clone());
    }
}

fn system(room: &mut Room, text: String) {
    let msg = json!({ "t": "chat", "id": room.next_msg, "system": true, "text": text, "ts": now() });
    room.next_msg += 1;
    push_history(room, msg.clone());
    broadcast(room, &msg);
}

fn push_history(room: &mut Room, msg: Value) {
    if room.history.len() >= HISTORY {
        room.history.pop_front();
    }
    room.history.push_back(msg);
}

fn serve_member(stream: TcpStream, room: &Arc<Mutex<Room>>, host_token: &str) {
    let mut reader = stream;
    reader.set_read_timeout(Some(Duration::from_secs(30))).ok();
    reader.set_nodelay(true).ok();

    // The first frame must be a hello with a name.
    let Ok((wire::JSON, body)) = wire::read(&mut reader) else { return };
    let Ok(hello) = serde_json::from_slice::<Value>(&body) else { return };
    if hello["t"] != "hello" {
        return;
    }
    let Ok(writer_stream) = reader.try_clone() else { return };
    let (tx, rx) = sync_channel::<Out>(QUEUE);
    wire::writer(writer_stream, rx);

    let id = {
        let mut r = room.lock().unwrap();
        let mut name = clean(hello["name"].as_str().unwrap_or(""), MAX_NAME);
        if name.is_empty() {
            name = "Guest".into();
        }
        let base = name.clone();
        let mut n = 2;
        while r.members.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            name = format!("{base} {n}");
            n += 1;
        }
        let id = r.next_id;
        r.next_id = r.next_id.wrapping_add(1).max(1);
        let host = hello["host"].as_str() == Some(host_token);
        r.members.push(Member {
            id,
            name: name.clone(),
            host,
            voice: false,
            muted: false,
            deaf: false,
            path: if host { "host".into() } else { "unknown".into() },
            rtt: None,
            tx: tx.clone(),
        });
        let welcome = json!({ "t": "welcome", "you": id, "name": name, "room": r.name, "history": r.history });
        send(&tx, wire::json(&welcome));
        let members = members_json(&r);
        broadcast(&r, &members);
        system(&mut r, format!("{name} joined the room"));
        id
    };

    reader.set_read_timeout(Some(IDLE)).ok();
    loop {
        let Ok((kind, body)) = wire::read(&mut reader) else { break };
        let mut r = room.lock().unwrap();
        let Some(me) = r.members.iter().position(|m| m.id == id) else { break };
        if kind == wire::AUDIO {
            let m = &r.members[me];
            if !m.voice || m.muted || body.len() < 8 {
                continue;
            }
            let frame = wire::audio(&id.to_be_bytes(), &body);
            for other in r.members.iter().filter(|o| o.id != id && o.voice && !o.deaf) {
                send(&other.tx, frame.clone());
            }
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        match msg["t"].as_str().unwrap_or("") {
            "chat" => {
                let text = clean(msg["text"].as_str().unwrap_or(""), MAX_TEXT);
                if text.is_empty() {
                    continue;
                }
                let name = r.members[me].name.clone();
                let out = json!({ "t": "chat", "id": r.next_msg, "from": id, "name": name, "text": text, "ts": now() });
                r.next_msg += 1;
                push_history(&mut r, out.clone());
                broadcast(&r, &out);
            }
            "voice" => {
                let on = msg["on"].as_bool().unwrap_or(false);
                if r.members[me].voice != on {
                    r.members[me].voice = on;
                    let members = members_json(&r);
                    broadcast(&r, &members);
                }
            }
            "mute" => {
                r.members[me].muted = msg["muted"].as_bool().unwrap_or(false);
                r.members[me].deaf = msg["deaf"].as_bool().unwrap_or(false);
                let members = members_json(&r);
                broadcast(&r, &members);
            }
            "status" => {
                if !r.members[me].host {
                    let path = match msg["path"].as_str() {
                        Some("direct") => "direct",
                        Some("relay") => "relay",
                        _ => "unknown",
                    };
                    r.members[me].path = path.into();
                    r.members[me].rtt = msg["rtt"].as_u64().map(|v| v.min(60_000) as u32);
                    let members = members_json(&r);
                    broadcast(&r, &members);
                }
            }
            "ping" => {
                let n = msg["n"].as_u64().unwrap_or(0);
                send(&r.members[me].tx, wire::json(&json!({ "t": "pong", "n": n })));
            }
            _ => {}
        }
    }

    // Gone: a closed connection, garbage, or silence past IDLE.
    let _ = reader.shutdown(Shutdown::Both);
    let mut r = room.lock().unwrap();
    if let Some(pos) = r.members.iter().position(|m| m.id == id) {
        let m = r.members.remove(pos);
        let _ = m.tx.try_send(Out::Close);
        let members = members_json(&r);
        broadcast(&r, &members);
        system(&mut r, format!("{} left the room", m.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean("  Bob\u{0007}  ", 32), "Bob");
        assert_eq!(clean(&"x".repeat(50), 32).len(), 32);
    }
}
