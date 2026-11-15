//! Listening on this PC only, the way TailCat looks for it.
//!
//! The official TailCat hands each connection from a friend to
//! `localhost:<port>`, and tries `::1` before `127.0.0.1`. So a server that
//! listens only on 127.0.0.1 gets each connection after a refused try on `::1`
//! (about 300 ms on Windows, where a refusal is slow), and a different program
//! listening on `[::1]` at the same port number would get the friend instead.
//! These listen on both, at the same port: on `::1` too whenever this PC has
//! IPv6, and never on a port another program already has on either.

use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

/// A free port on 127.0.0.1, with its `[::1]` twin if this PC has IPv6.
pub fn listen() -> io::Result<(TcpListener, Option<TcpListener>)> {
    let mut last = None;
    for _ in 0..20 {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = v4.local_addr()?.port();
        match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            Ok(v6) => return Ok((v4, Some(v6))),
            // Someone else has [::1] at this number: try another.
            Err(e) if e.kind() == ErrorKind::AddrInUse => last = Some(e),
            // No IPv6 here: TailCat only tries 127.0.0.1 then.
            Err(_) => return Ok((v4, None)),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(ErrorKind::AddrInUse, "no free port")))
}

/// "Share on my network" for a joined port: listens on every address of this
/// PC at `port` and passes each connection to 127.0.0.1:`to` (where TailCat's
/// near end is). Only the ports picked are shared this way; TailCat itself,
/// and KittyConnect's own check port beside them, stay on 127.0.0.1.
pub struct LanRelay {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LanRelay {
    pub fn start(port: u16, to: u16) -> io::Result<LanRelay> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                std::thread::spawn(move || {
                    if let Ok(near) = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, to)) {
                        pipe(conn, near);
                    }
                });
            }
        });
        Ok(LanRelay { port, stop })
    }
}

impl Drop for LanRelay {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Wakes the accept loop so it sees the flag and the port closes.
        let _ = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            std::time::Duration::from_millis(300),
        );
    }
}

/// Copies both ways until either side is done.
fn pipe(a: std::net::TcpStream, b: std::net::TcpStream) {
    use std::net::Shutdown;
    a.set_nodelay(true).ok();
    b.set_nodelay(true).ok();
    let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else { return };
    let up = std::thread::spawn(move || {
        let mut a = a;
        let mut b = b;
        let _ = io::copy(&mut a, &mut b);
        let _ = b.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut b2, &mut a2);
    let _ = a2.shutdown(Shutdown::Write);
    let _ = up.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn a_lan_relay_passes_both_ways_and_closes() {
        let far = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let to = far.local_addr().unwrap().port();
        let port = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap().local_addr().unwrap().port();
        let relay = LanRelay::start(port, to).unwrap();
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        c.write_all(b"ping").unwrap();
        let (mut s, _) = far.accept().unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ping");
        s.write_all(b"pong").unwrap();
        c.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"pong");
        drop(relay);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok(), "the port is free again");
    }

    #[test]
    fn both_loopbacks_reach_the_same_port() {
        let (v4, v6) = listen().unwrap();
        let port = v4.local_addr().unwrap().port();
        assert!(v4.local_addr().unwrap().ip().is_loopback());
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        c.write_all(b"4").unwrap();
        let mut b = [0u8; 1];
        v4.accept().unwrap().0.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"4");
        if let Some(v6) = v6 {
            assert_eq!(v6.local_addr().unwrap().port(), port);
            assert!(v6.local_addr().unwrap().ip().is_loopback());
            let mut c = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).unwrap();
            c.write_all(b"6").unwrap();
            v6.accept().unwrap().0.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"6");
        }
    }
}
