//! A member's connection to a room. The host connects over loopback; everyone
//! else connects to the local end of their `tailcat forward`, so the frames
//! cross the tunnel.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
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

pub struct Conn {
    pub tx: SyncSender<Out>,
    pub mixer: Arc<Mixer>,
    pub fetches: Arc<Mutex<HashMap<u32, Fetch>>>,
    alive: Arc<AtomicBool>,
}

impl Conn {
    pub fn send(&self, v: Value) {
        let _ = self.tx.try_send(Out::Frame(wire::json(&v)));
    }

    /// Asks the room for an attachment; it arrives in chunks on the reader.
    pub fn fetch(&self, aid: u32, to: Fetch) {
        self.fetches.lock().unwrap().insert(aid, to);
        self.send(json!({ "t": "attach.get", "aid": aid }));
    }

    /// Sends a file into chat from its own thread, waiting for room in the
    /// queue (never dropping chunks), with progress as it goes.
    pub fn upload(&self, tid: u32, path: PathBuf, text: String, gen: u64, proxy: EventLoopProxy<UserEvent>) {
        let tx = self.tx.clone();
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
        let _ = self.tx.try_send(Out::Close);
    }
}

fn emit(proxy: &EventLoopProxy<UserEvent>, gen: u64, ev: VoipEvent) {
    let _ = proxy.send_event(UserEvent::Voip(gen, ev));
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
    let (m, a, fx) = (mixer.clone(), alive.clone(), fetches.clone());
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
        emit(&proxy, gen, VoipEvent::Welcome(welcome));
        let Ok(w) = stream.try_clone() else { return };
        wire::writer(w, rx);

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
        let mut incoming: HashMap<u32, Incoming> = HashMap::new();
        loop {
            match wire::read(&mut stream) {
                Ok((wire::AUDIO, body)) if body.len() > 10 => {
                    let from = u16::from_be_bytes([body[0], body[1]]);
                    let seq = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                    m.push_packet(from, seq, &body[6..]);
                }
                Ok((wire::VIDEO, body)) if body.len() > 11 => {
                    let from = u16::from_be_bytes([body[0], body[1]]);
                    let key = body[2] & 1 == 1;
                    let ts = u64::from_be_bytes(body[3..11].try_into().unwrap_or([0; 8]));
                    emit(&proxy, gen, VoipEvent::Video { from, key, ts, data: body[11..].to_vec() });
                }
                Ok((wire::FILE, body)) if body.len() >= 4 => {
                    let aid = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    if let Some(inc) = incoming.get_mut(&aid) {
                        inc.got += (body.len() - 4) as u64;
                        if inc.file.write_all(&body[4..]).is_err() {
                            let inc = incoming.remove(&aid).unwrap();
                            let _ = std::fs::remove_file(&inc.part);
                            emit(&proxy, gen, VoipEvent::Fetched { aid, result: Err("Couldn't write the file.".into()), preview: inc.preview });
                        } else if !inc.preview && inc.last.elapsed() > Duration::from_millis(200) {
                            inc.last = Instant::now();
                            emit(&proxy, gen, VoipEvent::Fetching { aid, got: inc.got, size: inc.size });
                        }
                    }
                }
                Ok((wire::JSON, body)) => {
                    let Ok(v) = serde_json::from_slice::<Value>(&body) else { continue };
                    match v["t"].as_str().unwrap_or("") {
                        "attach.start" => {
                            let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                            let Some(to) = fx.lock().unwrap().remove(&aid) else { continue };
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
                                    incoming.insert(aid, Incoming { file, part, target, size, got: 0, preview: to.preview, last: Instant::now() });
                                }
                                Err(e) => emit(&proxy, gen, VoipEvent::Fetched { aid, result: Err(format!("Couldn't save: {e}")), preview: to.preview }),
                            }
                        }
                        "attach.done" => {
                            let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                            if let Some(inc) = incoming.remove(&aid) {
                                drop(inc.file);
                                let result = if inc.got == inc.size {
                                    std::fs::rename(&inc.part, &inc.target).map(|_| inc.target.clone()).map_err(|e| e.to_string())
                                } else {
                                    let _ = std::fs::remove_file(&inc.part);
                                    Err("The download was cut short.".into())
                                };
                                emit(&proxy, gen, VoipEvent::Fetched { aid, result, preview: inc.preview });
                            }
                        }
                        "attach.missing" => {
                            let aid = v["aid"].as_u64().unwrap_or(0) as u32;
                            let preview = fx.lock().unwrap().remove(&aid).is_some_and(|f| f.preview);
                            emit(&proxy, gen, VoipEvent::Fetched { aid, result: Err("That file is no longer in the room.".into()), preview });
                        }
                        t => {
                            let closed = t == "closed";
                            emit(&proxy, gen, VoipEvent::Message(v));
                            if closed {
                                break;
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    if a.load(Ordering::Relaxed) {
                        emit(&proxy, gen, VoipEvent::Failed("Lost the connection to the room.".into()));
                    }
                    break;
                }
            }
        }
        a.store(false, Ordering::Relaxed);
        for inc in incoming.into_values() {
            let _ = std::fs::remove_file(&inc.part);
        }
    });
    Conn { tx, mixer, fetches, alive }
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
        use std::io::Write;
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
}
