//! A member's connection to a room. The host connects over loopback; everyone
//! else connects to the local end of their `kc-voip forward`, so everything
//! crosses the tunnel.
//!
//! Two TCP connections per member: the main one (names, chat, control) and one
//! for attachments, so a download never queues in front of chat. Voice and
//! screen video are UDP datagrams on one socket to the same port (`media`),
//! with no TCP fallback.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use super::audio::Mixer;
use super::media;
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

/// The files connection's sender, while it's up.
#[derive(Default)]
pub struct Sides {
    files: Mutex<Option<SyncSender<Out>>>,
}

pub struct Conn {
    pub tx: SyncSender<Out>,
    pub mixer: Arc<Mixer>,
    pub fetches: Arc<Mutex<HashMap<u32, Fetch>>>,
    /// Voice and screen video out (UDP). None only if no UDP socket could be
    /// opened, in which case the connection reports a failure.
    pub media: Option<media::Sender>,
    /// What's being watched, shared with the media thread.
    pub link: Arc<media::Link>,
    sides: Arc<Sides>,
    alive: Arc<AtomicBool>,
}

impl Conn {
    pub fn send(&self, v: Value) {
        let _ = self.tx.try_send(Out::Frame(wire::json(&v)));
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
        self.link.stop();
        if let Some(tx) = self.sides.files.lock().unwrap().take() {
            let _ = tx.try_send(Out::Close);
        }
        let _ = self.tx.try_send(Out::Close);
    }
}

fn emit(proxy: &EventLoopProxy<UserEvent>, gen: u64, ev: VoipEvent) {
    let _ = proxy.send_event(UserEvent::Voip(gen, ev));
}

/// What one connection receives. Both connections handle every kind of
/// frame, so it doesn't matter which one the room sends something on; an
/// attachment always arrives whole on one of them.
struct Reader {
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
/// up (the first connection through TailCat can take a few seconds). Voice and
/// video go to the same port over UDP.
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
    let link: Arc<media::Link> = Arc::default();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let udp = udp_socket(addr);
    let media = udp.as_ref().ok().and_then(|u| u.try_clone().ok()).map(media::Sender::new);
    let (m, a, fx, sd, lk, keys) = (mixer.clone(), alive.clone(), fetches.clone(), sides.clone(), link.clone(), tx.clone());
    std::thread::spawn(move || {
        let udp = match udp {
            Ok(u) => u,
            Err(e) => {
                emit(&proxy, gen, VoipEvent::Failed(format!("Couldn't open a socket for voice: {e}")));
                return;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(75);
        let hello = json!({ "t": "hello", "v": super::server::PROTOCOL, "name": name, "host": host_token });
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
        let Some((id, key)) = udp_room(&welcome) else {
            emit(&proxy, gen, VoipEvent::Failed(
                "The host has an older KittyConnect. Ask them to update to 0.6 or newer (the Launcher can do it), then join again.".into(),
            ));
            return;
        };
        let Ok(w) = stream.try_clone() else { return };
        wire::writer(w, rx);

        // The files connection, before the room shows, so even the first
        // image previews come on it.
        if let Some(s) = open_side(addr, id as u64, key, "files") {
            let reader = Reader { fetches: fx.clone(), incoming: HashMap::new(), proxy: proxy.clone(), gen };
            side(s, 64, &sd.files, reader, a.clone(), sd.clone());
        }

        // Voice and video: register this socket with the room and keep it registered.
        let (ep, eg) = (proxy.clone(), gen);
        media::run_member(
            udp,
            id,
            key.to_string(),
            m.clone(),
            lk,
            move |sharer| {
                let _ = keys.try_send(Out::Frame(wire::json(&json!({ "t": "keyframe", "id": sharer }))));
            },
            move |ev| emit(&ep, eg, ev),
        );
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
        let mut reader = Reader { fetches: fx, incoming: HashMap::new(), proxy: proxy.clone(), gen };
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
        if let Some(tx) = sd.files.lock().unwrap().take() {
            let _ = tx.try_send(Out::Close);
        }
    });
    Conn { tx, mixer, fetches, media, link, sides, alive }
}

/// Our member id and key, if the room takes voice and video over UDP. A host
/// from before 0.6 expects them on TCP, and can't be joined.
fn udp_room(welcome: &Value) -> Option<(u16, &str)> {
    let id = welcome["you"].as_u64().filter(|v| *v <= u16::MAX as u64)? as u16;
    let key = welcome["key"].as_str().filter(|k| !k.is_empty())?;
    (welcome["udp"] == true).then_some((id, key))
}

/// This member's UDP socket for voice and video: on 127.0.0.1, connected to
/// the room's port (or the local end of the tunnel to it).
pub fn udp_socket(room: SocketAddr) -> std::io::Result<UdpSocket> {
    let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    s.connect(room)?;
    Ok(s)
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

/// Runs an accepted files connection: its writer, and a reader until it or
/// the member's main connection closes. Afterwards attachments go back to
/// the main connection.
fn side(mut s: TcpStream, cap: usize, slot: &Mutex<Option<SyncSender<Out>>>, mut reader: Reader, alive: Arc<AtomicBool>, sides: Arc<Sides>) {
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
        if let Some(tx) = sides.files.lock().unwrap().take() {
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
    use super::super::server::{RoomServer, PROTOCOL};
    use super::*;

    fn hello(name: &str, host: Option<&str>) -> Value {
        json!({ "t": "hello", "v": PROTOCOL, "name": name, "host": host })
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

    /// A member's UDP socket, registered with the room.
    fn udp_member(port: u16, welcome: &Value) -> UdpSocket {
        let u = udp_socket(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
        u.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        u.send(&media::register(welcome["you"].as_u64().unwrap() as u16, welcome["key"].as_str().unwrap())).unwrap();
        let mut b = [0u8; 16];
        let n = u.recv(&mut b).unwrap();
        assert_eq!(&b[..n], &[media::REGISTERED]);
        u
    }

    /// The next datagram of `kind`, waiting up to `within`. Windows reports
    /// an earlier datagram's "port unreachable" on the next receive as a
    /// connection reset; that isn't an answer, so it keeps waiting.
    fn udp_within(u: &UdpSocket, kind: u8, within: Duration) -> Option<Vec<u8>> {
        let until = std::time::Instant::now() + within;
        let mut b = vec![0u8; 2048];
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            u.set_read_timeout(Some(left)).unwrap();
            match u.recv(&mut b) {
                Ok(n) if b[0] == kind => return Some(b[..n].to_vec()),
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return None,
            }
        }
    }

    /// The next datagram of `kind`, which should come: up to 3 seconds, so a
    /// busy test machine doesn't fail it.
    fn next_udp(u: &UdpSocket, kind: u8) -> Option<Vec<u8>> {
        udp_within(u, kind, Duration::from_secs(3))
    }

    /// True if no datagram of `kind` comes within half a second.
    fn no_udp(u: &UdpSocket, kind: u8) -> bool {
        udp_within(u, kind, Duration::from_millis(500)).is_none()
    }

    /// A real room over loopback: two members, a chat message, and voice
    /// relayed from one to the other over UDP.
    #[test]
    fn room_end_to_end() {
        let room = RoomServer::start("Test room", std::env::temp_dir().join("kc-room-test-a")).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut a, wa) = try_join(addr, &hello("Ann", Some(&room.host_token))).unwrap();
        let (mut b, wb) = try_join(addr, &hello("Ann", None)).unwrap();
        assert_eq!(wa["room"], "Test room");
        assert_eq!(wb["name"], "Ann 2", "duplicate names get a number");
        assert_eq!(wa["udp"], true, "voice and video go over UDP");

        let members = next_json(&mut a, "members");
        assert!(members["members"][0]["host"].as_bool().unwrap());

        a.write_all_frame(&json!({ "t": "chat", "text": "hi there" })).unwrap();
        let mut msg = next_json(&mut b, "chat");
        while msg["system"] == true {
            msg = next_json(&mut b, "chat");
        }
        assert_eq!(msg["text"], "hi there");
        assert_eq!(msg["name"], "Ann");

        // Both join voice; A's packet reaches B over UDP, tagged with A's id.
        let (ua, ub) = (udp_member(room.port, &wa), udp_member(room.port, &wb));
        a.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        b.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let opus = vec![0x48u8; 120]; // stands in for an Opus packet
        ua.send(&media::voice(7, &opus)).unwrap();
        let got = next_udp(&ub, media::VOICE).expect("voice relayed over UDP");
        assert_eq!(media::u16_at(&got, 1), wa["you"].as_u64().unwrap() as u16);
        assert_eq!(media::u32_at(&got, 3), 7);
        assert_eq!(&got[7..], &opus[..]);
        assert!(no_udp(&ua, media::VOICE), "nobody hears themselves");

        // Ping answers with the same number.
        b.write_all_frame(&json!({ "t": "ping", "n": 42 })).unwrap();
        assert_eq!(next_json(&mut b, "pong")["n"], 42);

        // Ending the room tells everyone.
        drop(room);
        assert_eq!(next_json(&mut b, "closed")["t"], "closed");
    }

    /// An older KittyConnect (voice and video on TCP) is turned away with a
    /// reason, not let in to send what no one would hear.
    #[test]
    fn old_clients_are_told_to_update() {
        let room = RoomServer::start("Test", std::env::temp_dir().join("kc-room-test-old")).unwrap();
        for v in [1, 2] {
            let mut s = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, room.port))).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all_frame(&json!({ "t": "hello", "v": v, "name": "Old" })).unwrap();
            let bye = next_json(&mut s, "closed");
            assert!(bye["reason"].as_str().unwrap().contains("0.6 or newer"));
        }
    }

    /// Voice and video only come from an address registered with the right
    /// member key; anything else on the PC is ignored.
    #[test]
    fn udp_needs_registering_with_the_members_key() {
        let room = RoomServer::start("Test", std::env::temp_dir().join("kc-room-test-reg")).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut a, wa) = try_join(addr, &hello("Ann", None)).unwrap();
        let (mut b, wb) = try_join(addr, &hello("Bo", None)).unwrap();
        let ub = udp_member(room.port, &wb);
        a.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        b.write_all_frame(&json!({ "t": "voice", "on": true })).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        // A stranger's socket, and one with Ann's id but Bo's key.
        let stranger = udp_socket(addr).unwrap();
        stranger.send(&media::voice(1, &[1; 50])).unwrap();
        stranger.send(&media::register(wa["you"].as_u64().unwrap() as u16, wb["key"].as_str().unwrap())).unwrap();
        assert!(no_udp(&stranger, media::REGISTERED), "a wrong key isn't accepted");
        stranger.send(&media::voice(2, &[2; 50])).unwrap();
        assert!(no_udp(&ub, media::VOICE), "unregistered voice isn't relayed");

        // Ann registered: her voice gets through.
        let ua = udp_member(room.port, &wa);
        ua.send(&media::voice(3, &[3; 50])).unwrap();
        assert_eq!(media::u32_at(&next_udp(&ub, media::VOICE).unwrap(), 3), 3);
    }

    /// Attachments, screen video to watchers only (with resends and reports),
    /// and the host's mute and kick.
    #[test]
    fn room_files_screen_and_host_controls() {
        let dir = std::env::temp_dir().join(format!("kc-room-test-{}", std::process::id()));
        let room = RoomServer::start("Test", dir.clone()).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut host, wh) = try_join(addr, &hello("Hana", Some(&room.host_token))).unwrap();
        let (mut ben, wb) = try_join(addr, &hello("Ben", None)).unwrap();
        let (mut cy, wc) = try_join(addr, &hello("Cy", None)).unwrap();
        assert_eq!(wh["host"], true);
        assert_eq!(wb["host"], false);
        let ben_id = wb["you"].as_u64().unwrap();
        let cy_id = wc["you"].as_u64().unwrap() as u16;

        // Ben attaches a file on his files connection; it shows in chat for
        // everyone; Cy downloads it on hers.
        let mut ben_files = open_side(addr, ben_id, wb["key"].as_str().unwrap(), "files").unwrap();
        let mut cy_files = open_side(addr, cy_id as u64, wc["key"].as_str().unwrap(), "files").unwrap();
        assert!(open_side(addr, ben_id, wc["key"].as_str().unwrap(), "files").is_none(), "someone else's key is refused");
        assert!(open_side(addr, ben_id, wb["key"].as_str().unwrap(), "video").is_none(), "video has no TCP connection");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        ben_files.write_all_frame(&json!({ "t": "attach.begin", "tid": 1, "name": "pic.png", "size": payload.len() })).unwrap();
        for chunk in payload.chunks(wire::FILE_CHUNK) {
            send_raw(&mut ben_files, wire::binary(wire::FILE, &1u32.to_be_bytes(), chunk));
        }
        ben_files.write_all_frame(&json!({ "t": "attach.end", "tid": 1, "text": "look" })).unwrap();
        assert_eq!(next_json(&mut ben_files, "attach.ok")["tid"], 1);
        let mut msg = next_json(&mut cy, "chat");
        while msg["file"].is_null() {
            msg = next_json(&mut cy, "chat");
        }
        assert_eq!(msg["file"]["image"], true);
        assert_eq!(msg["text"], "look");
        let aid = msg["file"]["aid"].as_u64().unwrap();
        cy.write_all_frame(&json!({ "t": "attach.get", "aid": aid })).unwrap();
        assert_eq!(next_json(&mut cy_files, "attach.start")["size"], payload.len());
        let mut got = Vec::new();
        loop {
            let (kind, body) = wire::read(&mut cy_files).unwrap();
            if kind == wire::FILE {
                got.extend_from_slice(&body[4..]);
            } else if kind == wire::JSON && serde_json::from_slice::<Value>(&body).unwrap()["t"] == "attach.done" {
                break;
            }
        }
        assert_eq!(got, payload);

        // Ben shares his screen; only Cy watches, so only Cy gets the pieces,
        // and Ben is asked for a key frame when Cy starts watching.
        let (ub, uc, uh) = (udp_member(room.port, &wb), udp_member(room.port, &wc), udp_member(room.port, &wh));
        ben.write_all_frame(&json!({ "t": "screen", "on": true, "w": 1280, "h": 720, "fps": 30, "codec": "vp8" })).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        cy.write_all_frame(&json!({ "t": "watch", "id": ben_id })).unwrap();
        next_json(&mut ben, "keyframe");
        let frame: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let pieces = media::video_pieces(5, true, 123, &frame);
        assert_eq!(pieces.len(), 3);
        for p in &pieces {
            ub.send(p).unwrap();
        }
        let mut asm = media::Assembler::new(ben_id as u16, Instant::now());
        let mut out = Vec::new();
        for _ in 0..3 {
            out.extend(asm.datagram(&next_udp(&uc, media::VIDEO).expect("a piece for the watcher"), Instant::now()));
        }
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
        assert_eq!(out[0].ts, 123);
        assert_eq!(out[0].data, frame);
        assert!(no_udp(&uh, media::VIDEO), "the host isn't watching");

        // Cy asks for piece 1 again (a NACK); the room resends it from its buffer.
        let mut nack = vec![media::NACK];
        nack.extend_from_slice(&(ben_id as u16).to_be_bytes());
        nack.extend_from_slice(&5u32.to_be_bytes());
        nack.extend_from_slice(&1u16.to_be_bytes());
        uc.send(&nack).unwrap();
        let again = next_udp(&uc, media::VIDEO).expect("resent");
        assert_eq!(media::u16_at(&again, 2 + 5), 1, "piece 1");
        assert!(no_udp(&uc, media::VIDEO), "only what was asked for");
        // The host isn't watching Ben, so the room won't resend to them.
        uh.send(&nack).unwrap();
        assert!(no_udp(&uh, media::VIDEO));

        // Cy's report reaches Ben, tagged with Cy's id.
        uc.send(&asm.report()).unwrap();
        let (from, _) = media::Report::decode(&next_udp(&ub, media::REPORT).expect("report to the sharer")).unwrap();
        assert_eq!(from, cy_id);

        // Only the host can moderate: Cy's attempt does nothing.
        cy.write_all_frame(&json!({ "t": "mod", "action": "mute", "id": ben_id })).unwrap();
        // The host mutes Ben for everyone: Ben is told, the room is told, and his voice stops.
        for (s, on) in [(&mut ben, true), (&mut cy, true)] {
            s.write_all_frame(&json!({ "t": "voice", "on": on })).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        ub.send(&media::voice(1, &[5; 40])).unwrap();
        assert!(next_udp(&uc, media::VOICE).is_some(), "Cy hears Ben before the mute");
        host.write_all_frame(&json!({ "t": "mod", "action": "mute", "id": ben_id })).unwrap();
        assert!(next_json(&mut ben, "notice")["text"].as_str().unwrap().contains("muted you"));
        let mut sys = next_json(&mut cy, "chat");
        while !sys["text"].as_str().unwrap_or("").contains("for everyone") {
            sys = next_json(&mut cy, "chat");
        }
        assert_eq!(sys["text"], "Hana muted Ben for everyone");
        ub.send(&media::voice(2, &[5; 40])).unwrap();
        assert!(no_udp(&uc, media::VOICE), "muted for everyone: not relayed");
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
        ub.send(&media::voice(3, &[5; 40])).unwrap();
        assert!(no_udp(&uc, media::VOICE), "a kicked member's UDP is ignored");

        drop(room);
        assert!(!dir.join(format!("room-{}", addr.port())).exists(), "attachments are removed with the room");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A new member joining a room hosted by KittyConnect 0.5 (voice and video
    /// on TCP) is told why it can't work, instead of joining a silent room.
    #[test]
    fn an_old_host_is_detected() {
        let room = RoomServer::start("Test", std::env::temp_dir().join("kc-room-test-udp")).unwrap();
        let (_, w) = try_join(SocketAddr::from((Ipv4Addr::LOCALHOST, room.port)), &hello("Ann", None)).unwrap();
        assert!(udp_room(&w).is_some());
        // What a 0.5 room says: no "udp".
        let old = json!({ "t": "welcome", "you": 1, "name": "Ann", "room": "Old", "history": [], "host": false, "key": "k" });
        assert!(udp_room(&old).is_none());
    }

    /// Between a member and the room, drops every `nth` VIDEO datagram on its
    /// way to the member: a lossy link, for real.
    fn lossy_link(room: u16, nth: usize) -> u16 {
        let front = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = front.local_addr().unwrap().port();
        let back = udp_socket(SocketAddr::from((Ipv4Addr::LOCALHOST, room))).unwrap();
        let member: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        let (f2, m2) = (front.try_clone().unwrap(), member.clone());
        let b2 = back.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut b = vec![0u8; 2048];
            while let Ok((n, src)) = f2.recv_from(&mut b) {
                *m2.lock().unwrap() = Some(src);
                let _ = b2.send(&b[..n]);
            }
        });
        std::thread::spawn(move || {
            let mut b = vec![0u8; 2048];
            let mut video = 0;
            loop {
                let Ok(n) = back.recv(&mut b) else { continue };
                if b[0] == media::VIDEO {
                    video += 1;
                    if video % nth == 0 {
                        continue; // lost on the way
                    }
                }
                if let Some(to) = *member.lock().unwrap() {
                    let _ = front.send_to(&b[..n], to);
                }
            }
        });
        port
    }

    /// A screen share end to end, through a link that loses one video
    /// datagram in ten: the sharer's paced sender, the room, and the watcher's
    /// media thread together. Every frame still arrives whole and in order
    /// (missing pieces are asked for again), and the watcher's reports reach
    /// the sharer with the loss in them.
    #[test]
    fn a_screen_share_survives_a_lossy_link() {
        let room = RoomServer::start("Test", std::env::temp_dir().join("kc-room-test-share")).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut ann, wa) = try_join(addr, &hello("Ann", None)).unwrap();
        let (mut bo, wb) = try_join(addr, &hello("Bo", None)).unwrap();
        let ann_id = wa["you"].as_u64().unwrap() as u16;

        let (events, got) = std::sync::mpsc::channel::<(char, VoipEvent)>();
        let start = |who: char, sock: UdpSocket, w: &Value| {
            let link: Arc<media::Link> = Arc::default();
            let tx = events.clone();
            media::run_member(
                sock,
                w["you"].as_u64().unwrap() as u16,
                w["key"].as_str().unwrap().to_string(),
                Arc::new(Mixer::default()),
                link.clone(),
                |_| {},
                move |ev| {
                    let _ = tx.send((who, ev));
                },
            );
            link
        };
        let ann_sock = udp_socket(addr).unwrap();
        let sender = media::Sender::new(ann_sock.try_clone().unwrap());
        let ann_link = start('a', ann_sock, &wa);
        let bo_link = start('b', udp_socket(SocketAddr::from((Ipv4Addr::LOCALHOST, lossy_link(room.port, 10)))).unwrap(), &wb);

        let wait = |what: &dyn Fn(char, &VoipEvent) -> bool| loop {
            let (who, ev) = got.recv_timeout(Duration::from_secs(5)).expect("timed out");
            if what(who, &ev) {
                return ev;
            }
        };
        // Both registered, in either order.
        let up = std::cell::RefCell::new(std::collections::HashSet::new());
        while up.borrow().len() < 2 {
            wait(&|who, ev| matches!(ev, VoipEvent::Media("ok")) && up.borrow_mut().insert(who));
        }

        ann.write_all_frame(&json!({ "t": "screen", "on": true, "w": 1280, "h": 720, "fps": 30, "codec": "vp8" })).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        bo_link.watch(Some(ann_id));
        bo.write_all_frame(&json!({ "t": "watch", "id": ann_id })).unwrap();
        next_json(&mut ann, "keyframe");

        // 30 frames: a 60 KB key frame, then 8 KB deltas, at 30 fps.
        sender.set_rate(media::Sender::pace_for(4_000_000));
        let frames: Vec<Vec<u8>> = (0..30u32).map(|n| (0..if n == 0 { 60_000 } else { 8_000 }).map(|i| (i as u32 * 7 + n) as u8).collect()).collect();
        for (n, f) in frames.iter().enumerate() {
            assert!(sender.video(n == 0, media::video_pieces(n as u32, n == 0, n as u64 * 33_333, f)));
            std::thread::sleep(Duration::from_millis(33));
        }
        for (n, f) in frames.iter().enumerate() {
            match wait(&|who, ev| who == 'b' && matches!(ev, VoipEvent::Video { .. })) {
                VoipEvent::Video { from, key, ts, data } => {
                    assert_eq!((from, key, ts), (ann_id, n == 0, n as u64 * 33_333), "frame {n} in order");
                    assert!(data == *f, "frame {n} whole");
                }
                _ => unreachable!(),
            }
        }
        match wait(&|who, ev| who == 'a' && matches!(ev, VoipEvent::VideoReport { report, .. } if report.shown > 0)) {
            VoipEvent::VideoReport { watcher, report } => {
                assert_eq!(watcher, wb["you"].as_u64().unwrap() as u16);
                assert!(report.loss > 0.02, "the loss shows in the report: {report:?}");
            }
            _ => unreachable!(),
        }
        ann_link.stop();
        bo_link.stop();
    }
}
