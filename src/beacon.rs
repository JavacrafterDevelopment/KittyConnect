//! The port beacon: a tiny responder that an "Open a port" host serves through
//! TailCat beside the user's own ports, so everyone who joined can tell every
//! 30 seconds whether the host is still there, and the host can list them by
//! name with how each is connected (see [`crate::peers`]).
//!
//! TailCat 0.7 serves local ports under their own numbers (no remapping), so
//! the beacon listens on a fixed port the joiner knows to ask for. It only ever
//! answers a ping with a pong (the host's name, and whether the ports the
//! joiner typed are open on its code: see [`crate::peers`]), and a Minecraft
//! joiner's question about LAN worlds; it serves nothing else.
//!
//! TailCat reaches it as `localhost:47474`, trying `::1` before `127.0.0.1`,
//! so it listens on both: if another program already has either one, there is
//! no beacon (and port 47474 isn't served), rather than that program getting
//! the checks.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::OnceLock;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;

use crate::peers::Heard;
use crate::UserEvent;

/// The tunnel port the joiner forwards to. If something else on the host
/// already has it, the host goes without a beacon and the joiner just shows
/// the link as unchecked.
pub const PORT: u16 = 47474;

/// Starts the beacon once per run. Returns its port if it's listening.
pub fn ensure(proxy: &EventLoopProxy<UserEvent>) -> Option<u16> {
    static STARTED: OnceLock<Option<u16>> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, PORT)).ok()?;
        // No IPv6 on this PC is fine (TailCat then only tries 127.0.0.1);
        // someone else on [::1]:47474 isn't.
        let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, PORT)) {
            Ok(l) => Some(l),
            Err(e) if e.kind() == ErrorKind::AddrInUse => return None,
            Err(_) => None,
        };
        for listener in std::iter::once(v4).chain(v6) {
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let proxy = proxy.clone();
                    std::thread::spawn(move || serve_one(stream, &proxy));
                }
            });
        }
        Some(PORT)
    })
}

/// One check: a line in, at most a line out.
fn serve_one(mut stream: TcpStream, proxy: &EventLoopProxy<UserEvent>) {
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut line = String::new();
    let Ok(clone) = stream.try_clone() else { return };
    if BufReader::new(clone.take(1024)).read_line(&mut line).is_err() {
        return;
    }
    let (reply, heard) = answer(&line);
    if let Some(reply) = reply {
        let _ = stream.write_all(reply.as_bytes());
    }
    if let Some(heard) = heard {
        let _ = proxy.send_event(UserEvent::Heard { share: false, heard });
    }
}

/// The reply to one line (a pong for any ping, old or new), and who it was
/// from if they said.
fn answer(line: &str) -> (Option<String>, Option<Heard>) {
    let heard = Heard::parse(line);
    // A Minecraft joiner asking which LAN worlds to show (see `crate::lan`).
    if let Some(h) = heard.as_ref().filter(|h| h.op == "lan") {
        return (Some(crate::lan::reply(&h.addr)), None);
    }
    let check = heard.as_ref().and_then(|h| crate::peers::check_ports(&h.addr, &h.ports));
    let reply = line.contains("\"ping\"").then(|| crate::peers::pong(heard.as_ref().map_or(0, |h| h.peer), check));
    // Someone who typed a port this code didn't open isn't connected to it:
    // they aren't listed (they stop on this answer).
    let listed = heard.filter(Heard::is_named).filter(|_| check != Some(crate::peers::PortCheck::Wrong));
    (reply, listed)
}

#[cfg(test)]
mod tests {
    use super::answer;

    #[test]
    fn beacon_answers_old_and_new_pings() {
        let (reply, heard) = answer("{\"op\":\"ping\"}\n");
        assert!(reply.unwrap().contains("\"pong\":true") && heard.is_none());

        let line = r#"{"op":"ping","peer":7,"name":"Jo","path":"direct","rtt":12,"addr":"tcabc"}"#;
        let (reply, heard) = answer(line);
        assert!(reply.is_some());
        let h = heard.unwrap();
        assert_eq!((h.peer, h.addr.as_str(), h.path.as_str()), (7, "tcabc", "direct"));

        let (reply, heard) = answer(r#"{"op":"bye","peer":7,"addr":"tcabc"}"#);
        assert!(reply.is_none() && heard.unwrap().op == "bye");
        assert_eq!(answer("GET / HTTP/1.1").0, None);
        // Asking for LAN worlds on a code that isn't a Minecraft host: none.
        let (reply, heard) = answer(r#"{"op":"lan","addr":"tcnothing"}"#);
        assert_eq!(reply.unwrap().trim(), r#"{"lan":[]}"#);
        assert!(heard.is_none());
    }

    #[test]
    fn a_wrong_port_gets_wrong_and_is_not_listed() {
        crate::peers::serve("tcbeacon1", &[25565, super::PORT]);
        let ask = |ports: &str| answer(&format!(r#"{{"op":"ping","peer":9,"name":"Jo","path":"direct","addr":"tcbeacon1","ports":{ports}}}"#));
        let (reply, heard) = ask("[25566]");
        assert!(reply.unwrap().contains(r#""ports":"wrong""#));
        assert!(heard.is_none());
        let (reply, heard) = ask("[25565]");
        assert!(reply.unwrap().contains(r#""ports":"open""#));
        assert!(heard.is_some());
        crate::peers::unserve("tcbeacon1");
    }
}
