//! The room a VOIP host runs: one text channel, one voice channel, screen
//! shares, file attachments, and the list of who's here. It listens on
//! 127.0.0.1 only, TCP and UDP on the same port; friends reach it through
//! `kc-voip serve` (TailCat), so every byte between members crosses the
//! encrypted tunnel. The host joins their own room over loopback, which never
//! leaves the PC.
//!
//! TCP carries names, chat, control and attachments (see `wire`). Voice and
//! screen video are UDP datagrams only (see `media`), from members who have
//! registered their UDP address with the key from their welcome.
//!
//! Voice is relayed, not mixed: each speaker's packets go to everyone else in
//! voice, and each member mixes what they receive. Screen video goes only to
//! the members watching that share, and the room keeps the last second of it
//! to resend pieces a watcher missed. Each watcher (from 0.9) gets only the
//! frame-rate layers their connection keeps up with, so one slow connection
//! doesn't lower the picture for the others (see `media::Thinning`).
//!
//! The host (the member who presented the host token) can mute anyone for
//! everyone, and disconnect them. Everyone is told when that happens.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::media;
use super::wire::{self, Out};

const HISTORY: usize = 200;
const MAX_NAME: usize = 32;
const MAX_TEXT: usize = 2000;
/// Members ping every 30 seconds; one that's silent this long is gone.
const IDLE: Duration = Duration::from_secs(100);
const QUEUE: usize = 512;
/// Attachment chunks waiting for one member (2 MB).
const FILES_QUEUE: usize = 64;
/// A sharer is asked for a key frame at most this often because of drops.
const KEY_RETRY: Duration = Duration::from_secs(1);
/// The room protocol version members must speak (3: voice and video over UDP).
pub const PROTOCOL: u64 = 3;
/// How much of each screen share the room keeps, to resend missed pieces.
const VIDEO_KEEP: Duration = Duration::from_secs(1);
const VIDEO_KEEP_BYTES: usize = 8 * 1024 * 1024;
/// Largest single attachment, and all attachments in one room together.
pub const MAX_ATTACHMENT: u64 = 100 * 1024 * 1024;
const MAX_ROOM_FILES: u64 = 1024 * 1024 * 1024;

/// A side connection's sender, numbered so a closing connection only detaches
/// itself (not a newer one that replaced it).
type Side = Option<(u64, SyncSender<Out>)>;
static NEXT_SIDE: AtomicU64 = AtomicU64::new(1);

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
    /// When this sharer was last asked for a key frame.
    key_asked: Option<Instant>,
    /// Lets this member's files connection and UDP address attach to them.
    key: String,
    tx: SyncSender<Out>,
    files_tx: Side,
    /// Where their UDP (voice and video) comes from, once registered.
    udp: Option<SocketAddr>,
    /// The last second of their screen share.
    video: VideoBuffer,
    /// They understand frame-rate layers (0.9 and later), so they can be sent
    /// fewer frames when they fall behind. Older members get every frame.
    layers: bool,
    /// What they're sent of the share they're watching.
    thin: media::Thinning,
}

impl Member {
    fn files(&self) -> SyncSender<Out> {
        self.files_tx.as_ref().map(|s| s.1.clone()).unwrap_or_else(|| self.tx.clone())
    }

    /// Closes all their connections, after anything already queued.
    fn close(&self) {
        if let Some((_, t)) = &self.files_tx {
            let _ = t.try_send(Out::Close);
        }
        let _ = self.tx.try_send(Out::Close);
    }
}

/// Recent pieces of one screen share, as sent to watchers, for resending.
#[derive(Default)]
struct VideoBuffer {
    frames: VecDeque<SentFrame>,
    bytes: usize,
}

struct SentFrame {
    frame: u32,
    at: Instant,
    pieces: Vec<Option<Vec<u8>>>,
}

impl VideoBuffer {
    fn keep(&mut self, frame: u32, idx: u16, count: u16, datagram: &[u8]) {
        let now = Instant::now();
        if !self.frames.iter().any(|f| f.frame == frame) {
            self.frames.push_back(SentFrame { frame, at: now, pieces: vec![None; count as usize] });
        }
        let f = self.frames.iter_mut().rev().find(|f| f.frame == frame).unwrap();
        if let Some(slot) = f.pieces.get_mut(idx as usize).filter(|p| p.is_none()) {
            *slot = Some(datagram.to_vec());
            self.bytes += datagram.len();
        }
        while self.frames.front().is_some_and(|f| now.duration_since(f.at) > VIDEO_KEEP) || self.bytes > VIDEO_KEEP_BYTES {
            let Some(old) = self.frames.pop_front() else { break };
            self.bytes -= old.pieces.iter().flatten().map(Vec::len).sum::<usize>();
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }
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

/// A TCP listener and a UDP socket on the same 127.0.0.1 port.
pub fn bind_pair() -> Result<(TcpListener, UdpSocket), String> {
    let mut last = String::new();
    for _ in 0..20 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't open the room: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        match UdpSocket::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(udp) => return Ok((listener, udp)),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("Couldn't open the room for voice: {last}"))
}

impl RoomServer {
    pub fn start(name: &str, files_dir: PathBuf) -> Result<RoomServer, String> {
        let (listener, udp) = bind_pair()?;
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
        let (ur, us) = (room.clone(), stop.clone());
        std::thread::spawn(move || serve_udp(udp, &ur, &us));
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
                m.close();
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

/// Asks a sharer for a key frame: always when `now` (a new viewer), otherwise
/// at most once per `KEY_RETRY`, so a struggling viewer can't flood them.
fn ask_key(room: &mut Room, sharer: u16, now: bool) {
    let Some(s) = room.members.iter_mut().find(|m| m.id == sharer && m.screen.is_some()) else { return };
    if !now && s.key_asked.is_some_and(|t| t.elapsed() < KEY_RETRY) {
        return;
    }
    s.key_asked = Some(Instant::now());
    send(&s.tx, wire::json(&json!({ "t": "keyframe" })));
}

/// The room's UDP: voice and screen video, from registered members only.
fn serve_udp(sock: UdpSocket, room: &Mutex<Room>, stop: &AtomicBool) {
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let mut buf = vec![0u8; 65536];
    while !stop.load(Ordering::Relaxed) {
        // A timeout, or on Windows an ICMP "unreachable" from sending to a
        // member who has gone, reported as an error; the socket is still fine.
        let Ok((n, src)) = sock.recv_from(&mut buf) else { continue };
        if n == 0 || n > 1500 {
            continue;
        }
        let mut r = room.lock().unwrap();
        datagram(&mut r, &sock, src, &buf[..n]);
    }
}

fn datagram(r: &mut Room, sock: &UdpSocket, src: SocketAddr, d: &[u8]) {
    if d[0] == media::REGISTER {
        if d.len() < 4 {
            return;
        }
        let id = media::u16_at(d, 1);
        let key = &d[3..];
        let Some(m) = r.members.iter_mut().find(|m| m.id == id && !m.key.is_empty() && m.key.as_bytes() == key) else { return };
        m.udp = Some(src);
        // An address belongs to one member at a time.
        for o in r.members.iter_mut().filter(|o| o.id != id && o.udp == Some(src)) {
            o.udp = None;
        }
        let _ = sock.send_to(&[media::REGISTERED], src);
        return;
    }
    // Everything else only from a registered address.
    let Some(me) = r.members.iter().position(|m| m.udp == Some(src)) else { return };
    let id = r.members[me].id;
    match d[0] {
        media::VOICE if d.len() > 5 => {
            let m = &r.members[me];
            if !m.voice || m.muted || m.server_muted {
                return;
            }
            let mut out = Vec::with_capacity(d.len() + 2);
            out.push(media::VOICE);
            out.extend_from_slice(&id.to_be_bytes());
            out.extend_from_slice(&d[1..]);
            for o in r.members.iter().filter(|o| o.id != id && o.voice && !o.deaf) {
                if let Some(a) = o.udp {
                    let _ = sock.send_to(&out, a);
                }
            }
        }
        media::VIDEO => relay_video(r, sock, me, d),
        media::NACK if d.len() >= 7 => {
            let sharer = media::u16_at(d, 1);
            let frame = media::u32_at(d, 3);
            let Some(to) = r.members[me].udp.filter(|_| r.members[me].watching == Some(sharer)) else { return };
            let Some(s) = r.members.iter().find(|m| m.id == sharer) else { return };
            let Some(f) = s.video.frames.iter().find(|f| f.frame == frame) else { return };
            let wanted: Vec<usize> = d[7..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as usize).collect();
            if wanted.is_empty() {
                for p in f.pieces.iter().flatten() {
                    let _ = sock.send_to(p, to);
                }
            } else {
                for p in wanted.into_iter().filter_map(|i| f.pieces.get(i)).flatten() {
                    let _ = sock.send_to(p, to);
                }
            }
        }
        media::REPORT if d.len() >= 11 => {
            let sharer = media::u16_at(d, 1);
            if r.members[me].watching != Some(sharer) {
                return;
            }
            // How this watcher is doing decides what they're sent.
            if let Some((_, report)) = media::Report::decode(d).filter(|_| r.members[me].layers) {
                r.members[me].thin.report(&report);
            }
            if let Some(a) = r.members.iter().find(|m| m.id == sharer && m.screen.is_some()).and_then(|m| m.udp) {
                let mut out = d[..11].to_vec();
                out[1..3].copy_from_slice(&id.to_be_bytes());
                let _ = sock.send_to(&out, a);
            }
        }
        _ => {}
    }
}

/// One piece of a member's screen share: to each member watching it, and into
/// the buffer for resending.
fn relay_video(room: &mut Room, sock: &UdpSocket, me: usize, d: &[u8]) {
    if room.members[me].screen.is_none() {
        return;
    }
    let Some((frame, idx, count)) = media::video_head(d) else { return };
    let (key, layer) = media::video_layer(d);
    let id = room.members[me].id;
    let mut out = Vec::with_capacity(d.len() + 2);
    out.push(media::VIDEO);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&d[1..]);
    for o in room.members.iter_mut().filter(|o| o.watching == Some(id)) {
        // Only the layers this watcher keeps up with.
        if o.layers && !o.thin.pass(frame, key, layer) {
            continue;
        }
        if let Some(a) = o.udp {
            let _ = sock.send_to(&out, a);
        }
    }
    room.members[me].video.keep(frame, idx, count, &out);
}

/// An attachment on its way up from a member.
struct Upload {
    file: File,
    path: PathBuf,
    name: String,
    size: u64,
    got: u64,
}

/// A chunk of an upload. Written outside the room lock.
fn file_chunk(uploads: &mut HashMap<u32, Upload>, body: &[u8], reply: &SyncSender<Out>) {
    if body.len() < 4 {
        return;
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
        send(reply, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": why })));
    }
}

/// Uploads cut off when their connection closed.
fn drop_uploads(room: &Mutex<Room>, uploads: HashMap<u32, Upload>) {
    for u in uploads.into_values() {
        drop(u.file);
        let _ = std::fs::remove_file(&u.path);
        if let Ok(mut r) = room.lock() {
            r.stored = r.stored.saturating_sub(u.size);
        }
    }
}

/// attach.begin / attach.end / attach.get from member `me`. Replies about an
/// upload go back on the connection it came on (`reply`); a download is
/// streamed on the member's files connection.
fn attachment(r: &mut Room, me: usize, msg: &Value, uploads: &mut HashMap<u32, Upload>, reply: &SyncSender<Out>) {
    let id = r.members[me].id;
    match msg["t"].as_str().unwrap_or("") {
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
                send(reply, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": error })));
                return;
            }
            let path = r.dir.join(format!("up-{id}-{tid}.part"));
            match File::create(&path) {
                Ok(file) => {
                    r.stored += size;
                    uploads.insert(tid, Upload { file, path, name, size, got: 0 });
                }
                Err(_) => send(reply, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": "The host couldn't store the file." }))),
            }
        }
        "attach.end" => {
            let tid = msg["tid"].as_u64().unwrap_or(0) as u32;
            let Some(u) = uploads.remove(&tid) else { return };
            drop(u.file);
            if u.got != u.size {
                r.stored = r.stored.saturating_sub(u.size);
                let _ = std::fs::remove_file(&u.path);
                send(reply, wire::json(&json!({ "t": "attach.err", "tid": tid, "error": "The upload was cut short." })));
                return;
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
            push_history(r, out.clone());
            broadcast(r, &out);
            send(reply, wire::json(&json!({ "t": "attach.ok", "tid": tid })));
        }
        "attach.get" => {
            let aid = msg["aid"].as_u64().unwrap_or(0) as u32;
            let out = r.members[me].files();
            let Some(a) = r.attachments.get(&aid) else {
                send(&out, wire::json(&json!({ "t": "attach.missing", "aid": aid })));
                return;
            };
            let (path, name, size) = (a.path.clone(), a.name.clone(), a.size);
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
        _ => {}
    }
}

fn serve_member(stream: TcpStream, room: &Arc<Mutex<Room>>, host_token: &str) {
    let mut reader = stream;
    reader.set_read_timeout(Some(Duration::from_secs(30))).ok();
    reader.set_nodelay(true).ok();

    // The first frame must be a hello with a name, or a side connection.
    let Ok((wire::JSON, body)) = wire::read(&mut reader) else { return };
    let Ok(hello) = serde_json::from_slice::<Value>(&body) else { return };
    if hello["t"] == "channel" {
        return serve_side(reader, &hello, room);
    }
    if hello["t"] != "hello" {
        return;
    }
    // Voice and video moved to UDP in protocol 3; older versions send them on TCP.
    if hello["v"].as_u64().unwrap_or(1) < PROTOCOL {
        let bye = json!({ "t": "closed", "reason": "This room needs KittyConnect 0.6 or newer. Update KittyConnect (the Launcher can do it) and join again." });
        let _ = reader.write_all(&wire::json(&bye));
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
        let key = token();
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
            key_asked: None,
            key: key.clone(),
            tx: tx.clone(),
            files_tx: None,
            udp: None,
            video: VideoBuffer::default(),
            layers: hello["layers"] == true,
            thin: media::Thinning::default(),
        });
        // "udp": voice and video go as UDP datagrams to this port.
        let welcome = json!({ "t": "welcome", "you": id, "name": name, "room": r.name, "history": r.history, "host": host, "key": key, "udp": true, "v": PROTOCOL });
        send(&tx, wire::json(&welcome));
        let members = members_json(&r);
        broadcast(&r, &members);
        system(&mut r, format!("{name} joined the room"));
        id
    };

    let mut uploads: HashMap<u32, Upload> = HashMap::new();
    reader.set_read_timeout(Some(IDLE)).ok();
    loop {
        let Ok((kind, body)) = wire::read(&mut reader) else { break };

        // Attachments from a member without a files connection.
        if kind == wire::FILE {
            file_chunk(&mut uploads, &body, &tx);
            continue;
        }

        let mut r = room.lock().unwrap();
        let Some(me) = r.members.iter().position(|m| m.id == id) else { break };
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
                    r.members[me].video.clear();
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
                r.members[me].thin = media::Thinning::default();
                // A new viewer starts from a key frame.
                if let Some(t) = target {
                    ask_key(&mut r, t, true);
                }
            }
            "keyframe" => {
                if let Some(t) = msg["id"].as_u64().map(|v| v as u16) {
                    ask_key(&mut r, t, false);
                }
            }

            "attach.begin" | "attach.end" | "attach.get" => attachment(&mut r, me, &msg, &mut uploads, &tx),

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
                        m.close();
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
    drop_uploads(room, uploads);
    let mut r = room.lock().unwrap();
    if let Some(pos) = r.members.iter().position(|m| m.id == id) {
        let m = r.members.remove(pos);
        m.close();
        for o in r.members.iter_mut().filter(|o| o.watching == Some(id)) {
            o.watching = None;
        }
        let members = members_json(&r);
        broadcast(&r, &members);
        system(&mut r, format!("{} left the room", m.name));
    }
}

/// A member's files connection: attachments up and down, so a download never
/// queues in front of chat. It lasts as long as the member does.
fn serve_side(mut reader: TcpStream, hello: &Value, room: &Arc<Mutex<Room>>) {
    if hello["kind"].as_str() != Some("files") {
        return;
    }
    let id = hello["id"].as_u64().unwrap_or(0) as u16;
    let key = hello["key"].as_str().unwrap_or("");
    let Ok(writer_stream) = reader.try_clone() else { return };
    let (tx, rx) = sync_channel::<Out>(FILES_QUEUE);
    let serial = NEXT_SIDE.fetch_add(1, Ordering::Relaxed);
    {
        let mut r = room.lock().unwrap();
        let Some(m) = r.members.iter_mut().find(|m| m.id == id && !key.is_empty() && m.key == key) else { return };
        // Queued before the connection is handed out, so it's the first thing sent.
        send(&tx, wire::json(&json!({ "t": "channel.ok" })));
        if let Some((_, old)) = m.files_tx.replace((serial, tx.clone())) {
            let _ = old.try_send(Out::Close);
        }
    }
    wire::writer(writer_stream, rx);

    let mut uploads: HashMap<u32, Upload> = HashMap::new();
    reader.set_read_timeout(Some(IDLE)).ok();
    loop {
        let (kind, body) = match wire::read(&mut reader) {
            Ok(f) => f,
            // Quiet is normal here (no files); the main connection decides
            // when the member is gone.
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if room.lock().unwrap().members.iter().any(|m| m.id == id) {
                    continue;
                }
                break;
            }
            Err(_) => break,
        };
        if kind == wire::FILE {
            file_chunk(&mut uploads, &body, &tx);
            continue;
        }
        let mut r = room.lock().unwrap();
        let Some(me) = r.members.iter().position(|m| m.id == id) else { break };
        if let Ok(msg) = serde_json::from_slice::<Value>(&body) {
            attachment(&mut r, me, &msg, &mut uploads, &tx);
        }
    }

    let _ = reader.shutdown(Shutdown::Both);
    drop_uploads(room, uploads);
    let _ = tx.try_send(Out::Close);
    // Back to the main connection, unless a newer files connection took over.
    let mut r = room.lock().unwrap();
    if let Some(m) = r.members.iter_mut().find(|m| m.id == id) {
        if m.files_tx.as_ref().is_some_and(|(s, _)| *s == serial) {
            m.files_tx = None;
        }
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
