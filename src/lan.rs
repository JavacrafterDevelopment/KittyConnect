//! Minecraft LAN worlds, carried through the tunnel.
//!
//! Minecraft finds LAN worlds by a UDP message on port 4445, sent every 1.5
//! seconds by whoever has a world open to LAN (to the multicast group
//! 224.0.2.60):
//!
//!   [MOTD]Sam - New World[/MOTD][AD]25565[/AD]
//!
//! The Multiplayer screen lists it as a LAN world at the sender's address and
//! that port. So:
//!
//! - **Host**: KittyConnect listens for those messages from this PC's own
//!   Minecraft ([`listen`]), and the port beacon tells joiners, through the
//!   tunnel, which worlds are open on the ports it serves ([`reply`]).
//! - **Joiner**: KittyConnect asks the beacon every few seconds ([`ask`]) and
//!   sends the same message to its own Minecraft on 127.0.0.1, pointing at the
//!   local end of the tunnel ([`advertise`]). The world shows up under LAN,
//!   and joining it goes through the tunnel.
//!
//! Nothing here goes on the network outside this PC except through TailCat:
//! the listener only takes messages from this PC's own addresses, and the
//! joiner's announcements go to 127.0.0.1.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

pub const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 2, 60);
pub const PORT: u16 = 4445;
/// Minecraft announces every 1.5 seconds; a world not heard for this long is closed.
const FRESH: Duration = Duration::from_secs(5);
const EVERY: Duration = Duration::from_millis(1500);
/// What Minecraft shows of a MOTD is short anyway.
const MAX_MOTD: usize = 80;

/// One world open to LAN.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct World {
    pub port: u16,
    pub motd: String,
}

/// `[MOTD]…[/MOTD][AD]port[/AD]` → the world, if it is one.
pub fn parse(msg: &str) -> Option<World> {
    let motd = msg.split("[MOTD]").nth(1)?.split("[/MOTD]").next()?;
    let port = msg.split("[AD]").nth(1)?.split("[/AD]").next()?.trim().parse::<u16>().ok().filter(|p| *p > 0)?;
    Some(World { port, motd: clean_motd(motd) })
}

pub fn announcement(motd: &str, port: u16) -> String {
    format!("[MOTD]{}[/MOTD][AD]{port}[/AD]", clean_motd(motd))
}

/// No tags of our own inside it, nothing unprintable, not too long.
fn clean_motd(s: &str) -> String {
    let mut s: String = s.chars().filter(|c| !c.is_control()).collect();
    for tag in ["[MOTD]", "[/MOTD]", "[AD]", "[/AD]"] {
        s = s.replace(tag, "");
    }
    let s: String = s.trim().chars().take(MAX_MOTD).collect();
    if s.is_empty() {
        "Minecraft world".into()
    } else {
        s
    }
}

/// A socket on UDP 4445 that shares the port with Minecraft's own (which
/// listens there on the Multiplayer screen).
fn shared_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    // macOS (BSD) also wants this for two programs on one UDP port, as
    // Minecraft's own socket has it.
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    let s: UdpSocket = s.into();
    // Both the default interface and loopback, so it works with no network too.
    let _ = s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED);
    let _ = s.join_multicast_v4(&GROUP, &Ipv4Addr::LOCALHOST);
    Ok(s)
}

/// True if `ip` is one of this PC's own addresses (only something we can bind to is).
fn is_local(ip: IpAddr) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<IpAddr, bool>>> = OnceLock::new();
    if ip.is_loopback() {
        return true;
    }
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap();
    *seen.entry(ip).or_insert_with(|| UdpSocket::bind((ip, 0)).is_ok())
}

// ------------------------------------------------------------------- host

static HEARD: Mutex<Vec<(World, Instant)>> = Mutex::new(Vec::new());

/// Starts listening for this PC's own LAN worlds, once per run. Returns false
/// if UDP 4445 couldn't be opened.
pub fn listen() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let Ok(sock) = shared_socket() else { return false };
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                // Only worlds on this PC; a sibling's world on the same Wi-Fi isn't ours to share.
                if !is_local(from.ip()) {
                    continue;
                }
                let Some(world) = parse(&String::from_utf8_lossy(&buf[..n])) else { continue };
                let mut heard = HEARD.lock().unwrap();
                heard.retain(|(w, at)| w.port != world.port && at.elapsed() < FRESH);
                heard.push((world, Instant::now()));
            }
        });
        true
    })
}

/// Worlds this PC has open to LAN right now.
pub fn local_worlds() -> Vec<World> {
    let mut v: Vec<World> =
        HEARD.lock().unwrap().iter().filter(|(_, at)| at.elapsed() < FRESH).map(|(w, _)| w.clone()).collect();
    v.sort_by_key(|w| w.port);
    v
}

/// Minecraft hosts, by TailCat address: the ports each one opened, and a name
/// to show when a server is running there without announcing itself (a
/// dedicated server rather than Open to LAN).
static SERVING: Mutex<Vec<(String, Vec<u16>, String)>> = Mutex::new(Vec::new());

pub fn serve(addr: &str, ports: &[u16], fallback: &str) {
    let mut s = SERVING.lock().unwrap();
    s.retain(|(a, _, _)| a != addr);
    s.push((addr.to_string(), ports.to_vec(), fallback.to_string()));
}

pub fn unserve(addr: &str) {
    SERVING.lock().unwrap().retain(|(a, _, _)| a != addr);
}

/// The beacon's answer to `{"op":"lan","addr":…}`: the worlds open on the
/// ports that code serves. Nothing for a code that isn't a Minecraft host.
pub fn reply(addr: &str) -> String {
    let entry = SERVING.lock().unwrap().iter().find(|(a, _, _)| a == addr).cloned();
    let worlds: Vec<World> = match entry {
        Some((_, ports, fallback)) => {
            let open = local_worlds();
            ports
                .iter()
                .filter_map(|p| {
                    open.iter().find(|w| w.port == *p).cloned().or_else(|| {
                        let up = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, *p)), Duration::from_millis(200)).is_ok();
                        up.then(|| World { port: *p, motd: fallback.clone() })
                    })
                })
                .collect()
        }
        None => Vec::new(),
    };
    format!("{}\n", json!({ "lan": worlds }))
}

// ----------------------------------------------------------------- joiner

/// Asks a host's beacon (through the tunnel) which worlds are open.
pub fn ask(beacon: u16, addr: &str) -> Result<Vec<World>, String> {
    let mut s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, beacon)), Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(8))).ok();
    s.write_all(format!("{}\n", json!({ "op": "lan", "addr": addr })).as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s.take(16 * 1024)).read_line(&mut line).map_err(|e| e.to_string())?;
    #[derive(Deserialize)]
    struct Reply {
        lan: Vec<World>,
    }
    // An older host answers nothing useful: no worlds.
    Ok(serde_json::from_str::<Reply>(line.trim()).map(|r| r.lan).unwrap_or_default())
}

static ADVERTS: Mutex<Vec<(u16, String)>> = Mutex::new(Vec::new());

/// Shows these worlds in this PC's Minecraft, at 127.0.0.1 and the given
/// local ports (the near ends of tunnels). An empty list stops.
pub fn advertise(worlds: Vec<(u16, String)>) {
    *ADVERTS.lock().unwrap() = worlds;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| {
            let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) else { return };
            // Multicast out of loopback too, for a Minecraft that only hears the group.
            let _ = socket2::SockRef::from(&sock).set_multicast_if_v4(&Ipv4Addr::LOCALHOST);
            let _ = sock.set_multicast_loop_v4(true);
            loop {
                let worlds = ADVERTS.lock().unwrap().clone();
                for (port, motd) in worlds {
                    let msg = announcement(&motd, port);
                    // Minecraft's socket is on 0.0.0.0:4445, so a plain datagram
                    // to loopback reaches it, from 127.0.0.1: exactly where the tunnel is.
                    let _ = sock.send_to(msg.as_bytes(), (Ipv4Addr::LOCALHOST, PORT));
                    let _ = sock.send_to(msg.as_bytes(), (GROUP, PORT));
                }
                std::thread::sleep(EVERY);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_minecraft_announcements() {
        let w = parse("[MOTD]Sam - New World[/MOTD][AD]25565[/AD]").unwrap();
        assert_eq!(w, World { port: 25565, motd: "Sam - New World".into() });
        assert_eq!(parse(&announcement(&w.motd, 41000)).unwrap(), World { port: 41000, motd: w.motd.clone() });
        assert!(parse("[MOTD]x[/MOTD]").is_none());
        assert!(parse("[MOTD]x[/MOTD][AD]0[/AD]").is_none());
        assert!(parse("hello").is_none());
    }

    #[test]
    fn motd_cannot_smuggle_another_port() {
        let msg = announcement("evil[/MOTD][AD]1[/AD]\n", 25565);
        assert_eq!(parse(&msg).unwrap().port, 25565);
        assert!(!msg.contains('\n'));
        assert_eq!(parse(&announcement("", 1)).unwrap().motd, "Minecraft world");
    }

    #[test]
    fn beacon_only_tells_about_its_own_ports() {
        HEARD.lock().unwrap().push((World { port: 40001, motd: "Mine".into() }, Instant::now()));
        HEARD.lock().unwrap().push((World { port: 40002, motd: "Other".into() }, Instant::now()));
        serve("tcA", &[40001], "Sam's server");
        let r: serde_json::Value = serde_json::from_str(&reply("tcA")).unwrap();
        assert_eq!(r["lan"], json!([{ "port": 40001, "motd": "Mine" }]));
        // Not a Minecraft host: nothing.
        assert_eq!(reply("tcB").trim(), r#"{"lan":[]}"#);
        unserve("tcA");
        assert_eq!(reply("tcA").trim(), r#"{"lan":[]}"#);
    }

    #[test]
    fn a_running_server_that_does_not_announce_gets_the_fallback_name() {
        let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        serve("tcC", &[port], "Sam's server");
        let r: serde_json::Value = serde_json::from_str(&reply("tcC")).unwrap();
        assert_eq!(r["lan"][0]["motd"], "Sam's server");
        unserve("tcC");
    }

    /// The joiner's announcement really reaches a socket set up like
    /// Minecraft's, from 127.0.0.1.
    #[test]
    fn announcements_reach_a_local_listener() {
        let Ok(mc) = shared_socket() else { return }; // 4445 busy on this machine
        mc.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        advertise(vec![(41234, "Sam - Survival".into())]);
        let mut buf = [0u8; 512];
        let mut got = None;
        for _ in 0..10 {
            match mc.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Some(w) = parse(&String::from_utf8_lossy(&buf[..n])).filter(|w| w.port == 41234) {
                        got = Some((w, from.ip()));
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        advertise(Vec::new());
        let (w, ip) = got.expect("announcement heard");
        assert_eq!(w.motd, "Sam - Survival");
        assert!(ip.is_loopback());
    }
}
