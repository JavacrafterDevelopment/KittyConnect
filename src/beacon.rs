//! The port beacon: a tiny responder that an "Open a port" host serves through
//! TailCat beside the user's own ports, so everyone who joined can tell every
//! 30 seconds whether the host is still there, and the host can list them by
//! name with how each is connected (see [`crate::peers`]).
//!
//! TailCat 0.7 serves local ports under their own numbers (no remapping), so
//! the beacon listens on a fixed port the joiner knows to ask for. It only ever
//! answers a ping with a pong (and the host's name); it serves nothing else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
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
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, PORT)).ok()?;
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let proxy = proxy.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
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
                });
            }
        });
        Some(PORT)
    })
}

/// The reply to one line (a pong for any ping, old or new), and who it was
/// from if they said.
fn answer(line: &str) -> (Option<String>, Option<Heard>) {
    let heard = Heard::parse(line);
    // A Minecraft joiner asking which LAN worlds to show (see `crate::lan`).
    if let Some(h) = heard.as_ref().filter(|h| h.op == "lan") {
        return (Some(crate::lan::reply(&h.addr)), None);
    }
    let reply = line.contains("\"ping\"").then(|| crate::peers::pong(heard.as_ref().map_or(0, |h| h.peer)));
    (reply, heard.filter(Heard::is_named))
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
}
