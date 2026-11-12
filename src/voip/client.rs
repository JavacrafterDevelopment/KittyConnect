//! A member's connection to a room. The host connects over loopback; everyone
//! else connects to the local end of their `tailcat forward`, so the frames
//! cross the tunnel.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use super::audio::Mixer;
use super::wire::{self, Out};
use super::{adpcm, VoipEvent};
use crate::UserEvent;

pub struct Conn {
    pub tx: SyncSender<Out>,
    pub mixer: Arc<Mixer>,
    alive: Arc<AtomicBool>,
}

impl Conn {
    pub fn send(&self, v: Value) {
        let _ = self.tx.try_send(Out::Frame(wire::json(&v)));
    }
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
pub fn connect(port: u16, name: String, host_token: Option<String>, gen: u64, proxy: EventLoopProxy<UserEvent>) -> Conn {
    let (tx, rx) = sync_channel::<Out>(512);
    let mixer = Arc::new(Mixer::default());
    let alive = Arc::new(AtomicBool::new(true));
    let (m, a) = (mixer.clone(), alive.clone());
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(75);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let hello = json!({ "t": "hello", "v": 1, "name": name, "host": host_token });
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
        loop {
            match wire::read(&mut stream) {
                Ok((wire::AUDIO, body)) if body.len() > 10 => {
                    let from = u16::from_be_bytes([body[0], body[1]]);
                    if let Some(samples) = adpcm::decode(&body[6..]) {
                        m.push(from, &samples);
                    }
                }
                Ok((wire::JSON, body)) => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                        let closed = v["t"] == "closed";
                        emit(&proxy, gen, VoipEvent::Message(v));
                        if closed {
                            break;
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
    });
    Conn { tx, mixer, alive }
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
        let room = RoomServer::start("Test room").unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, room.port));
        let (mut a, wa) = try_join(addr, &json!({ "t": "hello", "name": "Ann", "host": room.host_token })).unwrap();
        let (mut b, wb) = try_join(addr, &json!({ "t": "hello", "name": "Ann" })).unwrap();
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
        let frame = adpcm::Encoder::default().encode(&[500i16; 320]);
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
}
