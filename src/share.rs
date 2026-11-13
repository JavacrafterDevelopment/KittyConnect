//! File sharing over a TailCat tunnel.
//!
//! The sharer runs a tiny file server on 127.0.0.1 and has TailCat serve that
//! one port. The receiver has TailCat forward a local port to it and talks to
//! the server through the tunnel. The protocol is one request per connection:
//!
//!   → {"op":"list"}\n              ← {"files":[{"id":1,"name":"a/b.txt","size":3}]}\n
//!   → {"op":"get","id":1}\n        ← {"ok":true,"size":3}\n  then 3 raw bytes
//!   → {"op":"ping",…}\n           ← {"pong":true,"name":…}\n  (see [`crate::peers`])
//!
//! Only files the sharer added are reachable, by id, never by path.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

const CHUNK: usize = 256 * 1024;
const TICK: Duration = Duration::from_millis(200);

// ------------------------------------------------------------------ sharing

#[derive(Clone, Serialize)]
pub struct Shared {
    pub id: u64,
    /// Shown to the peer; folders keep their inner layout ("Photos/2026/a.jpg").
    pub name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size: u64,
}

pub type Library = Arc<Mutex<Vec<Shared>>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Adds files, and every file inside any folder, to the shared list.
pub fn add_paths(lib: &Library, paths: &[PathBuf]) -> usize {
    let mut found = Vec::new();
    for p in paths {
        if p.is_dir() {
            let base = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Folder".into());
            walk(p, &base, &mut found);
        } else if let Ok(m) = std::fs::metadata(p) {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            found.push((name, p.clone(), m.len()));
        }
    }
    let mut lib = lib.lock().unwrap();
    let mut added = 0;
    for (name, path, size) in found {
        if lib.iter().any(|s| s.path == path) {
            continue;
        }
        lib.push(Shared { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), name, path, size });
        added += 1;
    }
    added
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = format!("{prefix}/{}", e.file_name().to_string_lossy());
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), &name, out);
        } else if ft.is_file() {
            if let Ok(m) = e.metadata() {
                out.push((name, e.path(), m.len()));
            }
        }
    }
}

pub enum ShareEvent {
    /// A peer is downloading `name`.
    Sending { conn: u64, name: String, done: u64, total: u64 },
    Sent { conn: u64, name: String, ok: bool },
}

pub struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
}

impl Server {
    pub fn start(lib: Library, proxy: EventLoopProxy<UserEvent>) -> Result<Server, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't open the file server: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            static CONN: AtomicU64 = AtomicU64::new(1);
            for stream in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (lib, proxy, flag) = (lib.clone(), proxy.clone(), flag.clone());
                let conn = CONN.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || {
                    let _ = serve_one(stream, conn, &lib, &proxy, &flag);
                });
            }
        });
        Ok(Server { port, stop })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_millis(300));
    }
}

#[derive(Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    id: u64,
}

fn read_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

fn serve_one(
    mut stream: TcpStream,
    conn: u64,
    lib: &Library,
    proxy: &EventLoopProxy<UserEvent>,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let line = read_line(&mut stream)?;
    let Ok(req) = serde_json::from_str::<Request>(&line) else {
        return Ok(());
    };
    // Who's checking in (or leaving), for the list of who's connected.
    if let Some(heard) = crate::peers::Heard::parse(&line).filter(crate::peers::Heard::is_named) {
        let _ = proxy.send_event(UserEvent::Heard { share: true, heard });
    }
    match req.op.as_str() {
        // The receiver checks every 30 seconds that sharing is still on.
        "ping" => stream.write_all(crate::peers::pong().as_bytes())?,
        "list" => {
            let files = lib.lock().unwrap().clone();
            let body = json!({ "v": 1, "files": files });
            stream.write_all(format!("{body}\n").as_bytes())?;
        }
        "get" => {
            let entry = lib.lock().unwrap().iter().find(|s| s.id == req.id).cloned();
            let opened = entry.as_ref().map(|e| File::open(&e.path));
            let (entry, mut file) = match (entry, opened) {
                (Some(e), Some(Ok(f))) => (e, f),
                _ => {
                    stream.write_all(b"{\"ok\":false,\"error\":\"That file isn't shared any more.\"}\n")?;
                    return Ok(());
                }
            };
            let total = file.metadata().map(|m| m.len()).unwrap_or(entry.size);
            stream.write_all(format!("{}\n", json!({ "ok": true, "size": total })).as_bytes())?;

            let send = |done, total| {
                let _ = proxy.send_event(UserEvent::Share(ShareEvent::Sending { conn, name: entry.name.clone(), done, total }));
            };
            send(0, total);
            let mut buf = vec![0u8; CHUNK];
            let (mut done, mut last) = (0u64, Instant::now());
            let result = loop {
                if stop.load(Ordering::Relaxed) {
                    break Err(std::io::Error::other("stopped"));
                }
                let n = match file.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => n,
                    Err(e) => break Err(e),
                };
                if let Err(e) = stream.write_all(&buf[..n]) {
                    break Err(e);
                }
                done += n as u64;
                if last.elapsed() >= TICK {
                    last = Instant::now();
                    send(done, total);
                }
            };
            let ok = result.is_ok() && done == total;
            let _ = stream.flush();
            let _ = proxy.send_event(UserEvent::Share(ShareEvent::Sent { conn, name: entry.name, ok }));
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------- receiving

#[derive(Clone, Serialize, Deserialize)]
pub struct Remote {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct ListReply {
    files: Vec<Remote>,
}

#[derive(Deserialize)]
struct GetReply {
    ok: bool,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    error: String,
}

pub enum RecvEvent {
    List(Result<Vec<Remote>, String>),
    Progress { id: u64, done: u64, total: u64 },
    Done { id: u64, result: Result<PathBuf, String> },
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Asks the sharer for its list, retrying while the tunnel comes up. The first
/// connection can take a while: TailCat bootstraps through a relay first.
pub fn fetch_list(port: u16, cancel: Arc<AtomicBool>, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(75);
        let mut last_err = String::from("Couldn't reach the sharer.");
        while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
            match list_once(port) {
                Ok(files) => {
                    let _ = proxy.send_event(UserEvent::Recv(RecvEvent::List(Ok(files))));
                    return;
                }
                Err(e) => last_err = e,
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        if !cancel.load(Ordering::Relaxed) {
            let msg = format!("{last_err} Check the code, and that the sharer still has sharing turned on.");
            let _ = proxy.send_event(UserEvent::Recv(RecvEvent::List(Err(msg))));
        }
    });
}

fn list_once(port: u16) -> Result<Vec<Remote>, String> {
    let mut s = TcpStream::connect_timeout(&local(port), Duration::from_secs(3)).map_err(|_| "Tunnel not up yet.".to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(b"{\"op\":\"list\"}\n").map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(s.take(64 * 1024 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|_| "The sharer didn't answer.".to_string())?;
    if line.trim().is_empty() {
        return Err("The sharer didn't answer.".into());
    }
    serde_json::from_str::<ListReply>(&line)
        .map(|r| r.files)
        .map_err(|_| "Got a reply that isn't from KittyConnect file sharing. Is this a port code?".to_string())
}

pub struct Job {
    pub file: Remote,
    pub dest: PathBuf,
}

/// One download at a time, in the order asked. Dropping the sender ends it.
pub fn downloader(port: u16, cancel: Arc<AtomicBool>, proxy: EventLoopProxy<UserEvent>) -> Sender<Job> {
    let (tx, rx) = channel::<Job>();
    std::thread::spawn(move || {
        for job in rx {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let id = job.file.id;
            let result = download(port, &job, &cancel, &proxy);
            let _ = proxy.send_event(UserEvent::Recv(RecvEvent::Done { id, result }));
        }
    });
    tx
}

fn download(port: u16, job: &Job, cancel: &AtomicBool, proxy: &EventLoopProxy<UserEvent>) -> Result<PathBuf, String> {
    let rel = safe_relative(&job.file.name).ok_or("The file name isn't safe to save.")?;
    let target = unique(&job.dest.join(rel));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't create {}: {e}", parent.display()))?;
    }

    let mut s = TcpStream::connect_timeout(&local(port), Duration::from_secs(10)).map_err(|e| format!("Lost the tunnel: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    s.write_all(format!("{}\n", json!({ "op": "get", "id": job.file.id })).as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::with_capacity(CHUNK, s);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|_| "The sharer didn't answer.".to_string())?;
    let reply: GetReply = serde_json::from_str(&line).map_err(|_| "The sharer sent a bad reply.".to_string())?;
    if !reply.ok {
        return Err(if reply.error.is_empty() { "The sharer refused.".into() } else { reply.error });
    }

    let part = target.with_file_name(format!("{}.part", target.file_name().unwrap_or_default().to_string_lossy()));
    let mut out = File::create(&part).map_err(|e| format!("Couldn't save to {}: {e}", part.display()))?;
    let total = reply.size;
    let mut buf = vec![0u8; CHUNK];
    let (mut done, mut last) = (0u64, Instant::now());
    let _ = proxy.send_event(UserEvent::Recv(RecvEvent::Progress { id: job.file.id, done: 0, total }));
    let result: Result<(), String> = loop {
        if done >= total {
            break Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            break Err("Cancelled.".into());
        }
        let want = buf.len().min((total - done) as usize);
        match reader.read(&mut buf[..want]) {
            Ok(0) => break Err("The connection dropped before the file finished.".into()),
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(format!("Couldn't write the file: {e}"));
                }
                done += n as u64;
            }
            Err(e) => break Err(format!("The connection dropped: {e}")),
        }
        if last.elapsed() >= TICK {
            last = Instant::now();
            let _ = proxy.send_event(UserEvent::Recv(RecvEvent::Progress { id: job.file.id, done, total }));
        }
    };
    drop(out);
    match result {
        Ok(()) => {
            std::fs::rename(&part, &target).map_err(|e| format!("Couldn't finish saving: {e}"))?;
            Ok(target)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Turns a peer-supplied name into a path that stays inside the inbox:
/// no `..`, no drive letters, no characters Windows can't store.
pub fn safe_relative(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        let part = part.trim();
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        let mut clean: String = part
            .chars()
            .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
            .collect();
        clean = clean.trim_end_matches(['.', ' ']).to_string();
        if clean.is_empty() {
            clean = "_".into();
        }
        let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
            "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            clean.insert(0, '_');
        }
        out.push(clean);
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `a.txt` → `a (1).txt` if `a.txt` is already there.
pub fn unique(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 1.. {
        let p = path.with_file_name(format!("{stem} ({n}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside() {
        assert_eq!(safe_relative("Photos/a.jpg"), Some(PathBuf::from("Photos").join("a.jpg")));
        assert_eq!(safe_relative("../x"), None);
        assert_eq!(safe_relative("a/../../x"), None);
        assert_eq!(safe_relative("C:\\Windows\\x"), Some(PathBuf::from("C_").join("Windows").join("x")));
        assert_eq!(safe_relative("/etc/passwd"), Some(PathBuf::from("etc").join("passwd")));
        assert_eq!(safe_relative("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(safe_relative(""), None);
    }
}
