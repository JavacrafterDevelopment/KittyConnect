//! The port beacon: a tiny responder that an "Open a port" host serves through
//! TailCat beside the user's own ports, so the friend who joined can tell
//! every 30 seconds whether the host is still there.
//!
//! TailCat 0.7 serves local ports under their own numbers (no remapping), so
//! the beacon listens on a fixed port the joiner knows to ask for. It only ever
//! answers `{"op":"ping"}` with `{"pong":true}`; it serves nothing else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::OnceLock;
use std::time::Duration;

/// The tunnel port the joiner forwards to. If something else on the host
/// already has it, the host goes without a beacon and the joiner just shows
/// the link as unchecked.
pub const PORT: u16 = 47474;

/// Starts the beacon once per run. Returns its port if it's listening.
pub fn ensure() -> Option<u16> {
    static STARTED: OnceLock<Option<u16>> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, PORT)).ok()?;
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut stream = stream;
                    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
                    let mut line = String::new();
                    let Ok(clone) = stream.try_clone() else { return };
                    if BufReader::new(clone.take(1024)).read_line(&mut line).is_ok() && line.contains("\"ping\"") {
                        let _ = stream.write_all(b"{\"pong\":true}\n");
                    }
                });
            }
        });
        Some(PORT)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn beacon_answers_ping() {
        let port = super::ensure().expect("beacon port free on the test machine");
        let rtt = crate::tailcat::ping_port(port, std::time::Duration::from_secs(3)).unwrap();
        assert!(rtt < std::time::Duration::from_secs(3));
    }
}
