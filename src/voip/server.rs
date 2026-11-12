//! The room a VOIP host runs: one text channel, one voice channel, screen
//! shares, file attachments, and the list of who's here. It listens on
//! 127.0.0.1 only; friends reach it through `tailcat serve`, so every byte
//! between members crosses the encrypted tunnel. The host joins their own room
//! over loopback, which never leaves the PC.
//!
//! Voice is relayed, not mixed: each speaker's frames go to everyone else in
//! voice, and each member mixes what they receive. Screen video goes only to
//! the members watching that share.
//!
//! The host (the member who presented the host token) can mute anyone for
//! everyone, and disconnect them. Everyone is told when that happens.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
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
/// The room protocol version members must speak (2: Opus voice).
pub const PROTOCOL: u64 = 2;
/// Largest single attachment, and all attachments in one room together.
pub const MAX_ATTACHMENT: u64 = 100 * 1024 * 1024;
const MAX_ROOM_FILES: u64 = 1024 * 1024 * 1024;

struct Member {
    id: u16,
    name: String,
    host: bool,
    voice: bool,
    muted: bool,
    deaf: bool,
    /// Muted for everyone by the host: their voice isn't relayed.
    server_muted: bool,
    path: String,
    rtt: Option<u32>,
    /// Set while they're sharing their screen: size, frame rate, codec.
    screen: Option<Value>,
    /// The member whose screen they're watching.
    watching: Option<u16>,
    tx: SyncSender<Out>,
}

struct Attachment {
    path: PathBuf,
    name: String,
    size: u64,
}

struct Room {
    name: String,
    members: Vec<Member>,
    history: VecDeque<Value>,
    next_id: u16,
    next_msg: u64,
    attachments: HashMap<u32, Attachment>,
    next_attachment: u32,
    stored: u64,
    dir: PathBuf,
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

pub fn is_image(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"].iter().any(|e| n.ends_with(e))
}

impl RoomServer {
    pub fn start(name: &str, files_dir: PathBuf) -> Result<RoomServer, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't open the room: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let dir = files_dir.join(format!("room-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make a folder for attachments: {e}"))?;
        let name = clean(name, 40);
        let room = Arc::new(Mutex::new(Room {
            name: if name.is_empty() { "Hangout".into() } else { name },
            members: Vec::new(),
            history: VecDeque::new(),
            next_id: 1,
            next_msg: 1,
            attachments: HashMap::new(),
            next_attachment: 1,
            stored: 0,
            dir,
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
            // Attachments only live as long as the room.
            let _ = std::fs::remove_dir_all(&room.dir);
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
            "muted": m.muted, "deaf": m.deaf, "smuted": m.server_muted,
            "path": m.path, "rtt": m.rtt, "screen": m.screen,
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

/// An attachment on its way up from a member.
struct Upload {
    file: File,
    path: PathBuf,
    name: String,
    size: u64,
    got: u64,
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
    // Voice changed from ADPCM to Opus in protocol 2; the two can't hear each other.
    if hello["v"].as_u64().unwrap_or(1) < PROTOCOL {
        let bye = json!({ "t": "closed", "reason": "This room needs KittyConnect 0.4 or newer. Update KittyConnect (the Launcher can do it) and join again." });
        let _ = reader.write_all(&wire::json(&bye));
        return;
    }
    let Ok(writer_stream) = reader.try_clone() else { return };
    let (tx, rx) = sync_channel::<Out>(QUEUE);
    wire::writer(writer_stream, rx);

    let (id, dir) = {
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
            server_muted: false,
            path: if host { "host".into() } else { "unknown".into() },
            rtt: None,
            screen: None,
            watching: None,
            tx: tx.clone(),
        });
        let welcome = json!({ "t": "welcome", "you": id, "name": name, "room": r.name, "history": r.history, "host": host });
        send(&tx, wire::json(&welcome));
        let members = members_json(&r);
        broadcast(&r, &members);
        system(&mut r, format!("{name} joined the room"));
        (id, r.dir.clone())
    };

    let mut uploads: HashMap<u32, Upload> = HashMap::new();
    reader.set_read_timeout(Some(IDLE)).ok();
    loop {
        let Ok((kind, body)) = wire::read(&mut reader) else { break };

        // Attachment chunks are written outside the room lock.
        if kind == wire::FILE {
            if body.len() < 4 {
                continue;
            }
            let tid = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            let mut failed = None;
            if let Some(u) = uploads.get_mut(&tid) {
                u.got += (body.len() - 4) as u64;
                if u.got > u.size {
                    failed = Some("The file was bigger than it said.");
                } else if u.file.write_all(&body[4..]).is_err() {
                    failed = Some("The host couldn't store the file.");
                }
            }
            if let Some(why) = failed {
                if let Some(u) = uploads.remove(&tid) {
                    let _ = std::fs::remove_file(&u.path);
                }
                send(&tx, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": why })));
            }
            continue;
        }

        let mut r = room.lock().unwrap();
        let Some(me) = r.members.iter().position(|m| m.id == id) else { break };
        if kind == wire::AUDIO {
            let m = &r.members[me];
            if !m.voice || m.muted || m.server_muted || body.len() < 8 {
                continue;
            }
            let frame = wire::audio(&id.to_be_bytes(), &body);
            for other in r.members.iter().filter(|o| o.id != id && o.voice && !o.deaf) {
                send(&other.tx, frame.clone());
            }
            continue;
        }
        if kind == wire::VIDEO {
            if r.members[me].screen.is_none() || body.len() < 10 {
                continue;
            }
            let frame = wire::binary(wire::VIDEO, &id.to_be_bytes(), &body);
            for other in r.members.iter().filter(|o| o.watching == Some(id)) {
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

            // ------------------------------------------------- screen share
            "screen" => {
                let was = r.members[me].screen.is_some();
                if msg["on"].as_bool() == Some(true) {
                    let w = msg["w"].as_u64().unwrap_or(0).min(7680);
                    let h = msg["h"].as_u64().unwrap_or(0).min(4320);
                    let fps = msg["fps"].as_u64().unwrap_or(30).clamp(1, 120);
                    let codec = clean(msg["codec"].as_str().unwrap_or(""), 40);
                    r.members[me].screen = Some(json!({ "w": w, "h": h, "fps": fps, "codec": codec }));
                    if !was {
                        let name = r.members[me].name.clone();
                        system(&mut r, format!("{name} started sharing their screen"));
                    }
                } else {
                    r.members[me].screen = None;
                    for o in r.members.iter_mut().filter(|o| o.watching == Some(id)) {
                        o.watching = None;
                    }
                }
                let members = members_json(&r);
                broadcast(&r, &members);
            }
            "watch" => {
                let target = msg["id"].as_u64().map(|v| v as u16).filter(|t| *t != id);
                r.members[me].watching = target;
                // A new viewer needs a key frame to start from.
                if let Some(s) = target.and_then(|t| r.members.iter().find(|m| m.id == t && m.screen.is_some())) {
                    send(&s.tx, wire::json(&json!({ "t": "keyframe" })));
                }
            }
            "keyframe" => {
                let target = msg["id"].as_u64().map(|v| v as u16);
                if let Some(s) = r.members.iter().find(|m| Some(m.id) == target && m.screen.is_some()) {
                    send(&s.tx, wire::json(&json!({ "t": "keyframe" })));
                }
            }

            // -------------------------------------------------- attachments
            "attach.begin" => {
                let tid = msg["tid"].as_u64().unwrap_or(0) as u32;
                let size = msg["size"].as_u64().unwrap_or(0);
                let name = clean(msg["name"].as_str().unwrap_or("file"), 120);
                let err = if size == 0 {
                    Some("That file is empty.".to_string())
                } else if size > MAX_ATTACHMENT {
                    Some(format!("Files up to {} MB can be sent in chat.", MAX_ATTACHMENT / 1024 / 1024))
                } else if r.stored + size > MAX_ROOM_FILES {
                    Some("This room has no room left for attachments.".to_string())
                } else {
                    None
                };
                if let Some(error) = err {
                    send(&tx, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": error })));
                    continue;
                }
                let path = dir.join(format!("up-{id}-{tid}.part"));
                match File::create(&path) {
                    Ok(file) => {
                        r.stored += size;
                        uploads.insert(tid, Upload { file, path, name, size, got: 0 });
                    }
                    Err(_) => send(&tx, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": "The host couldn't store the file." }))),
                }
            }
            "attach.end" => {
                let tid = msg["tid"].as_u64().unwrap_or(0) as u32;
                let Some(u) = uploads.remove(&tid) else { continue };
                drop(u.file);
                if u.got != u.size {
                    r.stored = r.stored.saturating_sub(u.size);
                    let _ = std::fs::remove_file(&u.path);
                    send(&tx, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": "The upload was cut short." })));
                    continue;
                }
                let aid = r.next_attachment;
                r.next_attachment += 1;
                let path = r.dir.join(format!("a{aid}"));
                let _ = std::fs::rename(&u.path, &path);
                r.attachments.insert(aid, Attachment { path, name: u.name.clone(), size: u.size });
                let text = clean(msg["text"].as_str().unwrap_or(""), MAX_TEXT);
                let name = r.members[me].name.clone();
                let out = json!({
                    "t": "chat", "id": r.next_msg, "from": id, "name": name, "text": text, "ts": now(),
                    "file": { "aid": aid, "name": u.name, "size": u.size, "image": is_image(&u.name) },
                });
                r.next_msg += 1;
                push_history(&mut r, out.clone());
                broadcast(&r, &out);
                send(&tx, wire::json(&json!({ "t": "attach.ok", "tid": tid })));
            }
            "attach.get" => {
                let aid = msg["aid"].as_u64().unwrap_or(0) as u32;
                let Some(a) = r.attachments.get(&aid) else {
                    send(&tx, wire::json(&json!({ "t": "attach.missing", "aid": aid })));
                    continue;
                };
                let (path, name, size, out) = (a.path.clone(), a.name.clone(), a.size, tx.clone());
                // Streamed from its own thread, waiting on the member's queue
                // rather than dropping chunks the way voice does.
                std::thread::spawn(move || {
                    let Ok(mut f) = File::open(&path) else { return };
                    let start = json!({ "t": "attach.start", "aid": aid, "name": name, "size": size });
                    if out.send(Out::Frame(wire::json(&start))).is_err() {
                        return;
                    }
                    let mut buf = vec![0u8; wire::FILE_CHUNK];
                    loop {
                        match f.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if out.send(Out::Frame(wire::binary(wire::FILE, &aid.to_be_bytes(), &buf[..n]))).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    let _ = out.send(Out::Frame(wire::json(&json!({ "t": "attach.done", "aid": aid }))));
                });
            }

            // ------------------------------------------------- host controls
            "mod" => {
                if !r.members[me].host {
                    continue;
                }
                let target = msg["id"].as_u64().unwrap_or(0) as u16;
                let Some(t) = r.members.iter().position(|m| m.id == target && !m.host) else { continue };
                let host_name = r.members[me].name.clone();
                let target_name = r.members[t].name.clone();
                match msg["action"].as_str().unwrap_or("") {
                    "mute" | "unmute" => {
                        let on = msg["action"] == "mute";
                        if r.members[t].server_muted == on {
                            continue;
                        }
                        r.members[t].server_muted = on;
                        let notice = if on {
                            format!("{host_name} muted you for everyone. Others can't hear you until they unmute you.")
                        } else {
                            format!("{host_name} unmuted you. Everyone can hear you again.")
                        };
                        send(&r.members[t].tx, wire::json(&json!({ "t": "notice", "text": notice })));
                        let members = members_json(&r);
                        broadcast(&r, &members);
                        let verb = if on { "muted" } else { "unmuted" };
                        system(&mut r, format!("{host_name} {verb} {target_name} for everyone"));
                    }
                    "kick" => {
                        // Removed here and now, rather than when their reader notices
                        // the closed socket (on Windows a blocked read may not wake).
                        // Anything more they send finds no member and ends their thread.
                        let m = r.members.remove(t);
                        let bye = json!({ "t": "closed", "reason": format!("{host_name} disconnected you from the room.") });
                        let _ = m.tx.try_send(Out::Frame(wire::json(&bye)));
                        let _ = m.tx.try_send(Out::Close);
                        for o in r.members.iter_mut().filter(|o| o.watching == Some(m.id)) {
                            o.watching = None;
                        }
                        let members = members_json(&r);
                        broadcast(&r, &members);
                        system(&mut r, format!("{target_name} was disconnected by the host"));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Gone: a closed connection, garbage or silence past IDLE. (A kicked member
    // was already removed and announced.)
    let _ = reader.shutdown(Shutdown::Both);
    for u in uploads.into_values() {
        let _ = std::fs::remove_file(&u.path);
        if let Ok(mut r) = room.lock() {
            r.stored = r.stored.saturating_sub(u.size);
        }
    }
    let mut r = room.lock().unwrap();
    if let Some(pos) = r.members.iter().position(|m| m.id == id) {
        let m = r.members.remove(pos);
        let _ = m.tx.try_send(Out::Close);
        for o in r.members.iter_mut().filter(|o| o.watching == Some(id)) {
            o.watching = None;
        }
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
        assert!(is_image("Shot.PNG") && !is_image("a.zip"));
    }
}
