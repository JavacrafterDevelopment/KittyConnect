//! A member's connection to a room. The host connects over loopback; everyone
//! else connects to the local end of their `tailcat forward`, so the frames
//! cross the tunnel.
//!
//! Three connections per member: the main one (names, chat, voice) and, when
//! the room supports them, one for screen video and one for attachments, so a
//! big key frame or a download never queues in front of voice.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use super::audio::Mixer;
use super::wire::{self, Out};
use super::VoipEvent;
use crate::UserEvent;

/// Where an attachment being fetched should go: the user's folder (a download)
/// or the image cache (an inline preview).
#[derive(Clone)]
pub struct Fetch {
    pub dir: PathBuf,
    pub preview: bool,
}

/// The side connections' senders, while they're up.
#[derive(Default)]
pub struct Sides {
    video: Mutex<Option<SyncSender<Out>>>,
    files: Mutex<Option<SyncSender<Out>>>,
}

pub struct Conn {
    pub tx: SyncSender<Out>,
    pub mixer: Arc<Mixer>,
    pub fetches: Arc<Mutex<HashMap<u32, Fetch>>>,
    sides: Arc<Sides>,
    alive: Arc<AtomicBool>,
}

impl Conn {
    pub fn send(&self, v: Value) {
        let _ = self.tx.try_send(Out::Frame(wire::json(&v)));
    }

    /// Where screen video goes: its own connection, or the main one if the
    /// room doesn't have side connections.
    pub fn video_tx(&self) -> SyncSender<Out> {
        self.sides.video.lock().unwrap().clone().unwrap_or_else(|| self.tx.clone())
    }

    fn files_tx(&self) -> SyncSender<Out> {
        self.sides.files.lock().unwrap().clone().unwrap_or_else(|| self.tx.clone())
    }

    /// Asks the room for an attachment; it arrives in chunks on the files
    /// connection (or the main one).
    pub fn fetch(&self, aid: u32, to: Fetch) {
        self.fetches.lock().unwrap().insert(aid, to);
        self.send(json!({ "t": "attach.get", "aid": aid }));
    }

    /// Sends a file into chat from its own thread, waiting for room in the
    /// queue (never dropping chunks), with progress as it goes.
    pub fn upload(&self, tid: u32, path: PathBuf, text: String, gen: u64, proxy: EventLoopProxy<UserEvent>) {
        let tx = self.files_tx();
        std::thread::spawn(move || {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            let fail = |e: String| emit(&proxy, gen, VoipEvent::Upload { tid, name: name.clone(), sent: 0, size: 0, error: Some(e) });
            let mut f = match File::open(&path) {
                Ok(f) => f,
                Err(e) => return fail(format!("Couldn't open {name}: {e}")),
            };
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            if size > super::server::MAX_ATTACHMENT {
                return fail(format!("{name} is too big. Files up to {} MB can be sent in chat.", super::server::MAX_ATTACHMENT / 1024 / 1024));
            }
            let begin = json!({ "t": "attach.begin", "tid": tid, "name": name, "size": size });
            if tx.send(Out::Frame(wire::json(&begin))).is_err() {
                return fail("Not connected to the room.".into());
            }
            let mut buf = vec![0u8; wire::FILE_CHUNK];
            let (mut sent, mut last) = (0u64, Instant::now());
            loop {
                let n = match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => return fail(format!("Couldn't read {name}: {e}")),
                };
                if tx.send(Out::Frame(wire::binary(wire::FILE, &tid.to_be_bytes(), &buf[..n]))).is_err() {
                    return fail("Lost the room while sending.".into());
                }
                sent += n as u64;
                if last.elapsed() > Duration::from_millis(200) {
                    last = Instant::now();
                    emit(&proxy, gen, VoipEvent::Upload { tid, name: name.clone(), sent, size, error: None });
                }
            }
            let end = json!({ "t": "attach.end", "tid": tid, "text": text });
            let _ = tx.send(Out::Frame(wire::json(&end)));
            emit(&proxy, gen, VoipEvent::Upload { tid, name, sent, size, error: None });
        });
    }
}

/// An attachment arriving from the room.
struct Incoming {
    file: File,
    part: PathBuf,
    target: PathBuf,
    size: u64,
    got: u64,
    preview: bool,
    last: Instant,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        for side in [&self.sides.video, &self.sides.files] {
            if let Some(tx) = side.lock().unwrap().take() {
                let _ = tx.try_send(Out::Close);
            }
        }
        let _ = self.tx.try_send(Out::Close);
    }
}

fn emit(proxy: &EventLoopProxy<UserEvent>, gen: u64, ev: VoipEvent) {
    let _ = proxy.send_event(UserEvent::Voip(gen, ev));
}

/// What one connection receives. Every connection handles every kind of
/// frame, so it doesn't matter which one the room sends something on; an
/// attachment always arrives whole on one of them.
struct Reader {
    mixer: Arc<Mixer>,
    fetches: Arc<Mutex<HashMap<u32, Fetch>>>,
    incoming: HashMap<u32, Incoming>,
    proxy: EventLoopProxy<UserEvent>,
    gen: u64,
}

impl Drop for Reader {
    fn drop(&mut self) {
        for inc in self.incoming.drain().map(|(_, i)| i) {
            drop(inc.file);
            let _ = std::fs::remove_file(&inc.part);
        }
    }
}

impl Reader {
    fn emit(&self, ev: VoipEvent) {
        emit(&self.proxy, self.gen, ev);
    }

    /// Handles one frame. False once the room has said it's closed.
    fn frame(&mut self, kind: u8, body: Vec<u8>) -> bool {
        match kind {
            wire::AUDIO if body.len() > 10 => {
                let from = u16::from_be_bytes([body[0], body[1]]);
                let seq = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                self.mixer.push_packet(from, seq, &body[6..]);
            }
            wire::VIDEO if body.len() > 11 => {
                let from = u16::from_be_bytes([body[0], body[1]]);
                let key = body[2] & 1 == 1;
                let ts = u64::from_be_bytes(body[3..11].try_into().unwrap_or([0; 8]));
                self.emit(VoipEvent::Video { from, key, ts, data: body[11..].to_vec() });
            }
            wire::FILE if body.len() >= 4 => {
                let aid = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                if let Some(inc) = self.incoming.get_mut(&aid) {
                    inc.got += (body.len() - 4) as u64;
                    if inc.file.write_all(&body[4..]).is_err() {
                        let inc = self.incoming.remove(&aid).unwrap();
                        drop(inc.file);
                        let _ = std::fs::remove_file(&inc.part);
                        self.emit(VoipEvent::Fetched { aid, result: Err("Couldn't write the file.".into()), preview: inc.preview });
                    } else if !inc.preview && inc.last.elapsed() > Duration::from_millis(200) {
                        inc.last = Instant::now();
                        let (got, size) = (inc.got, inc.size);
                        self.emit(VoipEvent::Fetching { aid, got, size });
                    }
                }
            }
            wire::JSON => {
                let Ok(v) = serde_json::from_slice::<Value>(&body) else { return true };
                match v["t"].as_str().unwrap_or("") {
                    "attach.start" => {
                        let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                        let Some(to) = self.fetches.lock().unwrap().remove(&aid) else { return true };
                        let name = v["name"].as_str().unwrap_or("file");
                        let size = v["size"].as_u64().unwrap_or(0);
                        let target = if to.preview {
                            to.dir.join(format!("a{aid}"))
                        } else {
                            let rel = crate::share::safe_relative(name).and_then(|p| p.file_name().map(PathBuf::from));
                            crate::share::unique(&to.dir.join(rel.unwrap_or_else(|| PathBuf::from("attachment"))))
                        };
                        let _ = std::fs::create_dir_all(&to.dir);
                        let part = target.with_extension("kcpart");
                        match File::create(&part) {
                            Ok(file) => {
                                self.incoming.insert(aid, Incoming { file, part, target, size, got: 0, preview: to.preview, last: Instant::now() });
                            }
                            Err(e) => self.emit(VoipEvent::Fetched { aid, result: Err(format!("Couldn't save: {e}")), preview: to.preview }),
                        }
                    }
                    "attach.done" => {
                        let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                        if let Some(inc) = self.incoming.remove(&aid) {
                            drop(inc.file);
                            let result = if inc.got == inc.size {
                                std::fs::rename(&inc.part, &inc.target).map(|_| inc.target.clone()).map_err(|e| e.to_string())
                            } else {
                                let _ = std::fs::remove_file(&inc.part);
                                Err("The download was cut short.".into())
                            };
                            self.emit(VoipEvent::Fetched { aid, result, preview: inc.preview });
                        }
                    }
                    "attach.missing" => {
                        let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                        let preview = self.fetches.lock().unwrap().remove(&aid).is_some_and(|f| f.preview);
                        self.emit(VoipEvent::Fetched { aid, result: Err("That file is no longer in the room.".into()), preview });
                    }
                    "channel.ok" => {}
                    t => {
                        let closed = t == "closed";
                        self.emit(VoipEvent::Message(v));
                        if closed {
                            return false;
                        }
                    }
                }
            }
            _ => {}
        }
        true
    }
}

/// Connects to the room on 127.0.0.1:`port`, retrying while the tunnel comes
/// up (the first connection through TailCat can take a few seconds).
pub fn connect(
    port: u16,
    name: String,
    host_token: Option<String>,
    settings: Arc<super::audio::Settings>,
    gen: u64,
    proxy: EventLoopProxy<UserEvent>,
) -> Conn {
    let (tx, rx) = sync_channel::<Out>(512);
    let mixer = Arc::new(Mixer::with_settings(settings));
    let alive = Arc::new(AtomicBool::new(true));
    let fetches: Arc<Mutex<HashMap<u32, Fetch>>> = Arc::default();
    let sides: Arc<Sides> = Arc::default();
    let (m, a, fx, sd) = (mixer.clone(), alive.clone(), fetches.clone(), sides.clone());
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(75);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let hello = json!({ "t": "hello", "v": 2, "name": name, "host": host_token });
        let (mut stream, welcome) = loop {
            if !a.load(Ordering::Relaxed) {
                return;
            }
            if Instant::now() > deadline {
                emit(&proxy, gen, VoipEvent::Failed(
                    "Couldn't reach the room. Check the code, and that the host still has it open.".into(),
                ));
                return;
            }
            if let Some(ok) = try_join(addr, &hello) {
                break ok;
            }
            std::thread::sleep(Duration::from_secs(1));
        };
        let Ok(w) = stream.try_clone() else { return };
        wire::writer(w, rx);

        // The side connections, before the room shows, so even the first
        // image previews come on the files connection. A room from before 0.5
        // sends no key, and everything stays on this connection.
        if let (Some(id), Some(key)) = (welcome["you"].as_u64(), welcome["key"].as_str()) {
            let opening: Vec<_> = ["video", "files"]
                .into_iter()
                .map(|kind| {
                    let key = key.to_string();
                    (kind, std::thread::spawn(move || open_side(addr, id, &key, kind)))
                })
                .collect();
            for (kind, t) in opening {
                let Some(s) = t.join().ok().flatten() else { continue };
                let video = kind == "video";
                let slot = if video { &sd.video } else { &sd.files };
                let cap = if video { super::server::VIDEO_QUEUE } else { 64 };
                let reader = Reader { mixer: m.clone(), fetches: fx.clone(), incoming: HashMap::new(), proxy: proxy.clone(), gen };
                side(s, cap, slot, reader, a.clone(), sd.clone(), video);
            }
        }
        emit(&proxy, gen, VoipEvent::Welcome(welcome));

        // Speaking lights: who the mixer heard lately, sent only on change.
        let (sm, sa, sp) = (m.clone(), a.clone(), proxy.clone());
        std::thread::spawn(move || {
            let mut last: Vec<u16> = Vec::new();
            while sa.load(Ordering::Relaxed) {
                let now = sm.speaking();
                if now != last {
                    emit(&sp, gen, VoipEvent::Speaking(now.clone()));
                    last = now;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });

        stream.set_read_timeout(Some(Duration::from_secs(100))).ok();
        let mut reader = Reader { mixer: m, fetches: fx, incoming: HashMap::new(), proxy: proxy.clone(), gen };
        loop {
            match wire::read(&mut stream) {
                Ok((kind, body)) => {
                    if !reader.frame(kind, body) {
                        break;
                    }
                }
                Err(_) => {
                    if a.load(Ordering::Relaxed) {
                        emit(&proxy, gen, VoipEvent::Failed("Lost the connection to the room.".into()));
                    }
                    break;
                }
            }
        }
        a.store(false, Ordering::Relaxed);
        for side in [&sd.video, &sd.files] {
            if let Some(tx) = side.lock().unwrap().take() {
                let _ = tx.try_send(Out::Close);
            }
        }
    });
    Conn { tx, mixer, fetches, sides, alive }
}

/// Opens a side connection and waits for the room to accept it.
fn open_side(addr: SocketAddr, id: u64, key: &str, kind: &str) -> Option<TcpStream> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.write_all(&wire::json(&json!({ "t": "channel", "kind": kind, "id": id, "key": key }))).ok()?;
    match wire::read(&mut s) {
        Ok((wire::JSON, body)) => {
            let v: Value = serde_json::from_slice(&body).ok()?;
            (v["t"] == "channel.ok").then_some(s)
        }
        _ => None,
    }
}

/// Runs an accepted side connection: its writer, and a reader until it or
/// the member's main connection closes. Afterwards its traffic goes back to
/// the main connection.
fn side(mut s: TcpStream, cap: usize, slot: &Mutex<Option<SyncSender<Out>>>, mut reader: Reader, alive: Arc<AtomicBool>, sides: Arc<Sides>, video: bool) {
    let Ok(w) = s.try_clone() else { return };
    let (tx, rx) = sync_channel::<Out>(cap);
    wire::writer(w, rx);
    *slot.lock().unwrap() = Some(tx);
    std::thread::spawn(move || {
        // Quiet is normal here; the main connection decides when the room is gone.
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        while alive.load(Ordering::Relaxed) {
            match wire::read(&mut s) {
                Ok((kind, body)) => {
                    if !reader.frame(kind, body) {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => continue,
                Err(_) => break,
            }
        }
        let slot = if video { &sides.video } else { &sides.files };
        if let Some(tx) = slot.lock().unwrap().take() {
            let _ = tx.try_send(Out::Close);
        }
    });
}

fn try_join(addr: SocketAddr, hello: &Value) -> Option<(TcpStream, Value)> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).ok()?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    s.write_all_frame(hello)?;
    match wire::read(&mut s) {
        Ok((wire::JSON, body)) => {
            let v: Value = serde_json::from_slice(&body).ok()?;
            (v["t"] == "welcome").then_some((s, v))
        }
        _ => None,
    }
}

trait WriteFrame {
    fn write_all_frame(&mut self, v: &Value) -> Option<()>;
}

impl WriteFrame for TcpStream {
    fn write_all_frame(&mut self, v: &Value) -> Option<()> {
        self.write_all(&wire::json(v)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::server::RoomServer;
    use super::*;

    /// A real room over loopback: two members, a chat message, and voice
    /// relayed from one to the other.
    #[test]
    fn room_end_to_end() {
        let room = RoomServer::start("Test room", std::env::temp_dir().join("kc-room-test-a")).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut a, wa) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Ann", "host": room.host_token })).unwrap();
        let (mut b, wb) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Ann" })).unwrap();
        assert_eq!(wa["room"], "Test room");
        assert_eq!(wb["name"], "Ann 2", "duplicate names get a number");

        let next_json = |s: &mut TcpStream, t: &str| loop {
            let (kind, body) = wire::read(s).unwrap();
            if kind == wire::JSON {
                let v: Value = serde_json::from_slice(&body).unwrap();
                if v["t"] == t {
                    return v;
                }
            }
        };
        let members = next_json(&mut a, "members");
        assert!(members["members"][0]["host"].as_bool().unwrap());

        a.write_all_frame(&json!({ "t": "chat", "text": "hi there" })).unwrap();
        let mut msg = next_json(&mut b, "chat");
        while msg["system"] == true {
            msg = next_json(&mut b, "chat");
        }
        assert_eq!(msg["text"], "hi there");
        assert_eq!(msg["name"], "Ann");

        // Both join voice; A's frame reaches B tagged with A's id.
        a.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        b.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let frame = vec![0x48u8; 120]; // stands in for an Opus packet
        {
            use std::io::Write;
            a.write_all(&wire::audio(&7u32.to_be_bytes(), &frame)).unwrap();
        }
        let got = loop {
            let (kind, body) = wire::read(&mut b).unwrap();
            if kind == wire::AUDIO {
                break body;
            }
        };
        assert_eq!(u16::from_be_bytes([got[0], got[1]]), wa["you"].as_u64().unwrap() as u16);
        assert_eq!(&got[6..], &frame[..]);

        // Ping answers with the same number.
        b.write_all_frame(&json!({ "t": "ping", "n": 42 })).unwrap();
        assert_eq!(next_json(&mut b, "pong")["n"], 42);

        // Ending the room tells everyone.
        drop(room);
        assert_eq!(next_json(&mut b, "closed")["t"], "closed");
    }

    fn next_json(s: &mut TcpStream, t: &str) -> Value {
        loop {
            let (kind, body) = wire::read(s).unwrap();
            if kind == wire::JSON {
                let v: Value = serde_json::from_slice(&body).unwrap();
                if v["t"] == t {
                    return v;
                }
            }
        }
    }

    fn send_raw(s: &mut TcpStream, bytes: Vec<u8>) {
        s.write_all(&bytes).unwrap();
    }

    /// An older KittyConnect (ADPCM voice) is turned away with a reason, not let in to garble the call.
    #[test]
    fn old_clients_are_told_to_update() {
        let room = RoomServer::start("Test", std::env::temp_dir().join("kc-room-test-old")).unwrap();
        let mut s = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, room.port))).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all_frame(&json!({ "t": "hello", "v": 1, "name": "Old" })).unwrap();
        let bye = next_json(&mut s, "closed");
        assert!(bye["reason"].as_str().unwrap().contains("0.4 or newer"));
    }

    /// Attachments, screen video to watchers only, and the host's mute and kick.
    #[test]
    fn room_files_screen_and_host_controls() {
        let dir = std::env::temp_dir().join(format!("kc-room-test-{}", std::process::id()));
        let room = RoomServer::start("Test", dir.clone()).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut host, wh) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Hana", "host": room.host_token })).unwrap();
        let (mut ben, wb) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Ben" })).unwrap();
        let (mut cy, _) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Cy" })).unwrap();
        assert_eq!(wh["host"], true);
        assert_eq!(wb["host"], false);
        let ben_id = wb["you"].as_u64().unwrap();

        // Ben attaches a file; it shows in chat for everyone; Cy downloads it.
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        ben.write_all_frame(&json!({ "t": "attach.begin", "tid": 1, "name": "pic.png", "size": payload.len() })).unwrap();
        for chunk in payload.chunks(wire::FILE_CHUNK) {
            send_raw(&mut ben, wire::binary(wire::FILE, &1u32.to_be_bytes(), chunk));
        }
        ben.write_all_frame(&json!({ "t": "attach.end", "tid": 1, "text": "look" })).unwrap();
        assert_eq!(next_json(&mut ben, "attach.ok")["tid"], 1);
        let mut msg = next_json(&mut cy, "chat");
        while msg["file"].is_null() {
            msg = next_json(&mut cy, "chat");
        }
        assert_eq!(msg["file"]["image"], true);
        assert_eq!(msg["text"], "look");
        let aid = msg["file"]["aid"].as_u64().unwrap();
        cy.write_all_frame(&json!({ "t": "attach.get", "aid": aid })).unwrap();
        assert_eq!(next_json(&mut cy, "attach.start")["size"], payload.len());
        let mut got = Vec::new();
        loop {
            let (kind, body) = wire::read(&mut cy).unwrap();
            if kind == wire::FILE {
                got.extend_from_slice(&body[4..]);
            } else if kind == wire::JSON && serde_json::from_slice::<Value>(&body).unwrap()["t"] == "attach.done" {
                break;
            }
        }
        assert_eq!(got, payload);

        // Ben shares his screen; only Cy watches, so only Cy gets video, and
        // Ben is asked for a key frame when Cy starts watching.
        ben.write_all_frame(&json!({ "t": "screen", "on": true, "w": 1280, "h": 720, "fps": 30, "codec": "vp8" })).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        cy.write_all_frame(&json!({ "t": "watch", "id": ben_id })).unwrap();
        next_json(&mut ben, "keyframe");
        let mut head = vec![1u8];
        head.extend_from_slice(&123u64.to_be_bytes());
        send_raw(&mut ben, wire::binary(wire::VIDEO, &head, &[7, 7, 7]));
        let v = loop {
            let (kind, body) = wire::read(&mut cy).unwrap();
            if kind == wire::VIDEO {
                break body;
            }
        };
        assert_eq!(u16::from_be_bytes([v[0], v[1]]) as u64, ben_id);
        assert_eq!(v[2], 1, "key frame flag");
        assert_eq!(&v[11..], &[7, 7, 7]);

        // Only the host can moderate: Cy's attempt does nothing.
        cy.write_all_frame(&json!({ "t": "mod", "action": "mute", "id": ben_id })).unwrap();
        // The host mutes Ben for everyone: Ben is told, the room is told, and his voice stops.
        host.write_all_frame(&json!({ "t": "mod", "action": "mute", "id": ben_id })).unwrap();
        assert!(next_json(&mut ben, "notice")["text"].as_str().unwrap().contains("muted you"));
        let mut sys = next_json(&mut cy, "chat");
        while !sys["text"].as_str().unwrap_or("").contains("for everyone") {
            sys = next_json(&mut cy, "chat");
        }
        assert_eq!(sys["text"], "Hana muted Ben for everyone");
        // Earlier member updates may still be queued; the newest shows the mute.
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        loop {
            let members = next_json(&mut host, "members");
            let ben_row = members["members"].as_array().unwrap().iter().find(|m| m["id"] == ben_id).cloned();
            if ben_row.is_some_and(|r| r["smuted"] == true) {
                break;
            }
        }

        // And disconnects him: Ben gets the reason, everyone sees why he left.
        host.write_all_frame(&json!({ "t": "mod", "action": "kick", "id": ben_id })).unwrap();
        assert!(next_json(&mut ben, "closed")["reason"].as_str().unwrap().contains("disconnected you"));
        let mut sys = next_json(&mut cy, "chat");
        while !sys["text"].as_str().unwrap_or("").contains("disconnected") {
            sys = next_json(&mut cy, "chat");
        }
        assert_eq!(sys["text"], "Ben was disconnected by the host");

        drop(room);
        assert!(!dir.join(format!("room-{}", addr.port())).exists(), "attachments are removed with the room");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Next frame of `kind`, or None if nothing comes within half a second.
    fn next_kind(s: &mut TcpStream, kind: u8) -> Option<Vec<u8>> {
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        loop {
            match wire::read(s) {
                Ok((k, body)) if k == kind => return Some(body),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    fn video_frame(key: bool, ts: u64) -> Vec<u8> {
        let mut head = vec![key as u8];
        head.extend_from_slice(&ts.to_be_bytes());
        wire::binary(wire::VIDEO, &head, &[5; 100])
    }

    /// Video and attachments travel on their own connections, never the main
    /// one that carries voice; a bad key gets no side connection; and a
    /// watcher who falls behind skips to the next key frame.
    #[test]
    fn side_connections_keep_video_and_files_off_voice() {
        let dir = std::env::temp_dir().join(format!("kc-room-test-side-{}", std::process::id()));
        let room = RoomServer::start("Test", dir.clone()).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut ann, wa) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Ann" })).unwrap();
        let (mut bo, wb) = try_join(addr, &json!({ "t": "hello", "v": 2, "name": "Bo" })).unwrap();
        let (ann_id, bo_id) = (wa["you"].as_u64().unwrap(), wb["you"].as_u64().unwrap());
        let (ann_key, bo_key) = (wa["key"].as_str().unwrap(), wb["key"].as_str().unwrap());

        assert!(open_side(addr, bo_id, ann_key, "video").is_none(), "someone else's key is refused");
        let mut ann_video = open_side(addr, ann_id, ann_key, "video").unwrap();
        let mut bo_video = open_side(addr, bo_id, bo_key, "video").unwrap();
        let mut bo_files = open_side(addr, bo_id, bo_key, "files").unwrap();

        // Ann shares on her video connection; Bo watches. Frames before the
        // first key frame are skipped (Bo's decoder couldn't start from them).
        ann.write_all_frame(&json!({ "t": "screen", "on": true, "w": 1280, "h": 720, "fps": 30, "codec": "vp8" })).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        bo.write_all_frame(&json!({ "t": "watch", "id": ann_id })).unwrap();
        next_json(&mut ann, "keyframe");
        send_raw(&mut ann_video, video_frame(false, 1));
        send_raw(&mut ann_video, video_frame(true, 2));
        send_raw(&mut ann_video, video_frame(false, 3));
        let first = next_kind(&mut bo_video, wire::VIDEO).unwrap();
        assert_eq!(first[2], 1, "starts at the key frame");
        assert_eq!(u64::from_be_bytes(first[3..11].try_into().unwrap()), 2);
        let second = next_kind(&mut bo_video, wire::VIDEO).unwrap();
        assert_eq!(u64::from_be_bytes(second[3..11].try_into().unwrap()), 3);
        assert!(next_kind(&mut bo, wire::VIDEO).is_none(), "no video on the main connection");

        // Bo stops reading video. Past his queue frames are dropped, Ann is
        // asked for a key frame, and nothing is sent to Bo until one comes.
        // (Past the once-a-second limit since Bo started watching.)
        std::thread::sleep(Duration::from_millis(1100));
        for ts in 10..(10 + super::super::server::VIDEO_QUEUE as u64 + 400) {
            send_raw(&mut ann_video, wire::binary(wire::VIDEO, &[0], &[&ts.to_be_bytes()[..], &[5; 20_000]].concat()));
        }
        next_json(&mut ann, "keyframe");

        // Bo's attachment goes up and comes back down on his files connection.
        let payload = vec![42u8; 70_000];
        bo_files.write_all_frame(&json!({ "t": "attach.begin", "tid": 9, "name": "notes.txt", "size": payload.len() })).unwrap();
        for chunk in payload.chunks(wire::FILE_CHUNK) {
            send_raw(&mut bo_files, wire::binary(wire::FILE, &9u32.to_be_bytes(), chunk));
        }
        bo_files.write_all_frame(&json!({ "t": "attach.end", "tid": 9, "text": "" })).unwrap();
        assert_eq!(next_json(&mut bo_files, "attach.ok")["tid"], 9);
        let mut msg = next_json(&mut ann, "chat");
        while msg["file"].is_null() {
            msg = next_json(&mut ann, "chat");
        }
        bo.write_all_frame(&json!({ "t": "attach.get", "aid": msg["file"]["aid"] })).unwrap();
        assert_eq!(next_json(&mut bo_files, "attach.start")["size"], payload.len());
        let mut got = 0;
        while got < payload.len() {
            got += next_kind(&mut bo_files, wire::FILE).unwrap().len() - 4;
        }
        assert!(next_kind(&mut bo, wire::FILE).is_none(), "no file chunks on the main connection");

        drop(room);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
