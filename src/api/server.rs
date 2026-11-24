//! The game API's listener: 127.0.0.1 only, one JSON object per line each
//! way. Connections only read and write here; what they say is decided on
//! the app's own thread ([`super::Api`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::{ApiEvent, Emit, MAX_CONNS, MAX_LINE, PORTS, VERSION};

/// The game API listening on this PC.
pub struct ApiServer {
    pub port: u16,
    stop: Arc<AtomicBool>,
    open: Arc<Mutex<HashMap<u64, TcpStream>>>,
}

impl ApiServer {
    /// Listens on the first free port of [`PORTS`], on 127.0.0.1 only:
    /// nothing on the network can reach it.
    pub fn start(emit: Emit) -> Result<ApiServer, String> {
        let listener = PORTS
            .clone()
            .find_map(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, p)).ok())
            .ok_or_else(|| format!("Ports {} to {} are all taken by other programs.", PORTS.start(), PORTS.end()))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let open: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::default();
        let (flag, conns) = (stop.clone(), open.clone());
        std::thread::spawn(move || accept_loop(listener, port, flag, conns, emit));
        Ok(ApiServer { port, stop, open })
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for (_, s) in self.open.lock().unwrap().drain() {
            let _ = s.shutdown(Shutdown::Both);
        }
        // Wakes the accept loop so it sees the flag and the port closes.
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_millis(300));
    }
}

fn accept_loop(listener: TcpListener, port: u16, stop: Arc<AtomicBool>, open: Arc<Mutex<HashMap<u64, TcpStream>>>, emit: Emit) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        // Bound to 127.0.0.1, so this is always true; checked anyway.
        let Ok(peer) = stream.peer_addr() else { continue };
        if !peer.ip().is_loopback() {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        {
            let mut o = open.lock().unwrap();
            if o.len() >= MAX_CONNS {
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
            if let Ok(s) = stream.try_clone() {
                o.insert(id, s);
            }
        }
        let (open, emit) = (open.clone(), emit.clone());
        std::thread::spawn(move || {
            serve(id, stream, peer.port(), port, &emit);
            open.lock().unwrap().remove(&id);
            emit(ApiEvent::Closed { conn: id });
        });
    }
}

/// One line of at most MAX_LINE bytes. None at the end, on an error or a
/// timeout, or for a line too long; Some(Null) for one that isn't JSON.
fn read_line(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut buf = Vec::new();
    let n = reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf).ok()?;
    if n == 0 || buf.last() != Some(&b'\n') {
        return None;
    }
    Some(serde_json::from_slice(&buf).unwrap_or(Value::Null))
}

fn serve(id: u64, stream: TcpStream, theirs: u16, ours: u16, emit: &Emit) {
    stream.set_nodelay(true).ok();
    let (Ok(mut writer), Ok(read_half)) = (stream.try_clone(), stream.try_clone()) else { return };
    let mut reader = BufReader::new(read_half);
    let (tx, rx) = channel::<Value>();
    // Everything to this program goes through one writer, in order. Null
    // closes the connection once what's before it has gone.
    let writer_thread = std::thread::spawn(move || {
        while let Ok(v) = rx.recv() {
            if v.is_null() {
                break;
            }
            let mut line = v.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });
    // Says what this is, so a game looking through the ports knows it found Portside.
    let _ = tx.send(json!({ "op": "portside", "v": VERSION, "version": crate::VERSION }));

    // The first line must be a hello, soon. A web page can't open a plain
    // connection like this, and what a browser sends (an HTTP request)
    // isn't JSON, so it's closed here at once.
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    match read_line(&mut reader) {
        Some(hello) if hello["op"] == "hello" => {
            let exe = super::peer::program(theirs, ours);
            emit(ApiEvent::Open { conn: id, tx: tx.clone(), exe, hello });
        }
        _ => {
            let _ = tx.send(Value::Null);
            drop(tx);
            let _ = writer_thread.join();
            return;
        }
    }
    stream.set_read_timeout(None).ok();
    while let Some(msg) = read_line(&mut reader) {
        if !msg.is_object() {
            let _ = tx.send(json!({ "op": "error", "error": "Each line must be one JSON object." }));
            continue;
        }
        emit(ApiEvent::Msg { conn: id, msg });
    }
    let _ = tx.send(Value::Null);
    drop(tx);
    let _ = writer_thread.join();
}
