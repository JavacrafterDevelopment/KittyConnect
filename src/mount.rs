//! Mounts: a folder shared over TailCat that friends open as a drive (a letter
//! in Explorer, a volume in Finder's sidebar), or browse inside KittyConnect.
//!
//! The host runs a WebDAV server on 127.0.0.1 ([`MountServer`], dav-server on
//! a small runtime of its own) and `tailcat serve`s that one port, beside the
//! beacon (for the 30-second check and who's connected). WebDAV is what
//! Windows ("Map network drive") and macOS ("Connect to Server") speak out of
//! the box, so the friend installs nothing. Everything between them crosses
//! the encrypted tunnel; the server is never reachable from the network.
//!
//! ```text
//! /<Name>/…                    the folder, over WebDAV (Name: the folder's name, URL-safe)
//! /.kittyconnect/info          {"name", "write"}
//! /.kittyconnect/list?path=…   {"entries": [{"name", "dir", "size", "modified"}]}  (the built-in browser)
//! ```
//!
//! Read only unless the host ticked "Let them add, change and delete files and
//! folders": then all of WebDAV, with locks (Finder and Office lock before
//! writing). The choice is the host's, enforced here. Every path is checked to
//! stay inside the folder, symlinks and all ([`Contained`]).

use std::convert::Infallible;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dav_server::body::Body;
use dav_server::davpath::DavPath;
use dav_server::memls::MemLs;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, DavProp, FsError, FsFuture, FsStream, OpenOptions, ReadDirMeta,
};
use dav_server::localfs::LocalFs;
use dav_server::{DavHandler, DavMethodSet};
use futures_util::{future, StreamExt};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// What the friend's side learns about a mount.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Info {
    /// The folder's part of the URL (what the drive or volume is called).
    pub name: String,
    /// The folder's own name, to show ("Trip 2026" for "Trip-2026").
    #[serde(default)]
    pub title: String,
    pub write: bool,
}

/// One thing in a folder, for the built-in browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: u64,
}

/// The folder's name as the last part of the mount's URL: what Explorer and
/// Finder show the drive as. Letters, digits and `-_.` are kept; anything else
/// becomes `-`.
pub fn url_name(dir: &Path) -> String {
    let raw = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    // No leading dot, so it can never be the server's own /.kittyconnect/.
    let s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    if s.is_empty() {
        "Shared".into()
    } else {
        s.chars().take(60).collect()
    }
}

// ------------------------------------------------------------------- guard

/// Whether `full` is inside `root` (both resolved: `..`, links, case on
/// Windows). Something that doesn't exist yet (about to be created) counts as
/// inside if the folder it would go in is, and it isn't a link to nowhere.
pub fn inside(root: &Path, full: &Path) -> bool {
    // A `..` anywhere is refused outright: through a folder that doesn't
    // exist, it couldn't be resolved to check.
    if full.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }
    match std::fs::canonicalize(full) {
        Ok(real) => real.starts_with(root),
        Err(_) => {
            if std::fs::symlink_metadata(full).is_ok() {
                return false; // a link whose target is gone (or can't be reached)
            }
            full.ancestors().skip(1).find_map(|a| std::fs::canonicalize(a).ok()).is_some_and(|real| real.starts_with(root))
        }
    }
}

/// dav-server's local folder, kept inside the shared folder: a link in it that
/// leads elsewhere (to the rest of the drive, say) is neither listed nor
/// followed, and nothing can be created through one.
#[derive(Clone)]
struct Contained {
    fs: Box<LocalFs>,
    root: PathBuf,
}

impl Contained {
    fn new(root: PathBuf) -> Contained {
        Contained { fs: LocalFs::new(&root, false, cfg!(windows), true), root }
    }

    fn ok(&self, p: &DavPath) -> bool {
        inside(&self.root, &self.root.join(p.as_rel_ospath()))
    }
}

fn forbidden<'a, T: Send + 'a>() -> FsFuture<'a, T> {
    Box::pin(future::ready(Err(FsError::Forbidden)))
}

impl DavFileSystem for Contained {
    fn open<'a>(&'a self, path: &'a DavPath, options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.open(path, options)
    }

    fn read_dir<'a>(&'a self, path: &'a DavPath, meta: ReadDirMeta) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        if !self.ok(path) {
            return forbidden();
        }
        Box::pin(async move {
            let entries = self.fs.read_dir(path, meta).await?;
            let (root, dir) = (self.root.clone(), self.root.join(path.as_rel_ospath()));
            let kept = entries.filter(move |e| {
                let keep = match e {
                    Ok(e) => inside(&root, &dir.join(name_path(&e.name()))),
                    Err(_) => true,
                };
                future::ready(keep)
            });
            Ok(Box::pin(kept) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.metadata(path)
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.symlink_metadata(path)
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.create_dir(path)
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.remove_dir(path)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.remove_file(path)
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        if !self.ok(from) || !self.ok(to) {
            return forbidden();
        }
        self.fs.rename(from, to)
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        if !self.ok(from) || !self.ok(to) {
            return forbidden();
        }
        self.fs.copy(from, to)
    }

    fn set_accessed<'a>(&'a self, path: &'a DavPath, tm: SystemTime) -> FsFuture<'a, ()> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.set_accessed(path, tm)
    }

    fn set_modified<'a>(&'a self, path: &'a DavPath, tm: SystemTime) -> FsFuture<'a, ()> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.set_modified(path, tm)
    }

    fn have_props<'a>(&'a self, path: &'a DavPath) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        self.fs.have_props(path)
    }

    fn patch_props<'a>(&'a self, path: &'a DavPath, patch: Vec<(bool, DavProp)>) -> FsFuture<'a, Vec<(StatusCode, DavProp)>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.patch_props(path, patch)
    }

    fn get_props<'a>(&'a self, path: &'a DavPath, do_content: bool) -> FsFuture<'a, Vec<DavProp>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.get_props(path, do_content)
    }

    fn get_prop<'a>(&'a self, path: &'a DavPath, prop: DavProp) -> FsFuture<'a, Vec<u8>> {
        if !self.ok(path) {
            return forbidden();
        }
        self.fs.get_prop(path, prop)
    }

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        self.fs.get_quota()
    }
}

/// A directory entry's name (bytes) as a path part.
fn name_path(name: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(name))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(name).to_string())
    }
}

// ------------------------------------------------------------------ server

/// The host's side: the WebDAV server for one shared folder. Stops when dropped.
pub struct MountServer {
    pub port: u16,
    pub name: String,
    rt: Option<tokio::runtime::Runtime>,
}

struct Api {
    root: PathBuf,
    name: String,
    title: String,
    write: bool,
}

impl MountServer {
    pub fn start(dir: &Path, write: bool) -> Result<MountServer, String> {
        let root = std::fs::canonicalize(dir).map_err(|e| format!("Couldn't open {}: {e}", dir.display()))?;
        if !root.is_dir() {
            return Err(format!("{} isn't a folder.", dir.display()));
        }
        let name = url_name(&root);
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't start the mount's server: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kc-mount")
            .enable_all()
            .build()
            .map_err(|e| format!("Couldn't start the mount's server: {e}"))?;

        let mut dav = DavHandler::builder()
            .filesystem(Box::new(Contained::new(root.clone())))
            .strip_prefix(format!("/{name}"))
            .methods(if write { DavMethodSet::WEBDAV_RW } else { DavMethodSet::WEBDAV_RO });
        // Real locks, kept in memory while the share is on: Finder and Office
        // lock before writing, and two friends can't overwrite each other's
        // open file. (A lock left by someone who vanished times out.)
        if write {
            dav = dav.locksystem(MemLs::new());
        }
        let dav = dav.build_handler();
        let title: String = root.file_name().map(|n| n.to_string_lossy().chars().filter(|c| !c.is_control()).take(80).collect()).unwrap_or_default();
        let api = Arc::new(Api { root, name: name.clone(), title, write });

        rt.spawn(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let (dav, api) = (dav.clone(), api.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let (dav, api) = (dav.clone(), api.clone());
                        async move { Ok::<_, Infallible>(route(&dav, &api, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Ok(MountServer { port, name, rt: Some(rt) })
    }
}

impl Drop for MountServer {
    /// Stops the server: connections end and the port closes within a moment.
    /// (The tunnel to it is closed with the share, so nothing reaches it
    /// meanwhile.) Done off the UI thread, as a file being written finishes first.
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            std::thread::spawn(move || rt.shutdown_timeout(Duration::from_secs(2)));
        }
    }
}

fn reply(status: StatusCode, mime: &str, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", mime)
        .header("Content-Length", body.len().to_string())
        .body(body.into())
        .unwrap()
}

async fn route(dav: &DavHandler, api: &Api, req: Request<Incoming>) -> Response<Body> {
    let path = req.uri().path().to_string();
    if let Some(what) = path.strip_prefix("/.kittyconnect/") {
        return api.handle(what, req.uri().query().unwrap_or(""));
    }
    let prefix = format!("/{}", api.name);
    if path == prefix || path.starts_with(&format!("{prefix}/")) {
        return dav.handle(req).await;
    }
    // The server's root, which Windows and Finder look at first: a WebDAV
    // folder holding just the shared one.
    if path == "/" {
        let allow = if api.write { "OPTIONS, GET, HEAD, PROPFIND, PROPPATCH, PUT, DELETE, MKCOL, COPY, MOVE, LOCK, UNLOCK" } else { "OPTIONS, GET, HEAD, PROPFIND" };
        if req.method() == Method::OPTIONS {
            return Response::builder()
                .status(StatusCode::OK)
                .header("DAV", if api.write { "1, 2" } else { "1" })
                .header("MS-Author-Via", "DAV")
                .header("Allow", allow)
                .header("Content-Length", "0")
                .body(Body::empty())
                .unwrap();
        }
        if req.method().as_str() == "PROPFIND" {
            let depth0 = req.headers().get("Depth").is_some_and(|d| d.as_bytes() == b"0");
            let collection = |href: &str| {
                format!(
                    "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
                )
            };
            let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">");
            xml += &collection("/");
            if !depth0 {
                xml += &collection(&format!("{prefix}/"));
            }
            xml += "</D:multistatus>";
            return reply(StatusCode::MULTI_STATUS, "application/xml; charset=utf-8", xml);
        }
    }
    reply(StatusCode::NOT_FOUND, "text/plain", "Not found".into())
}

impl Api {
    fn handle(&self, what: &str, query: &str) -> Response<Body> {
        match what {
            "info" => reply(StatusCode::OK, "application/json", json!({ "name": self.name, "title": self.title, "write": self.write }).to_string()),
            "list" => {
                let rel = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("path="))
                    .map(|v| percent_decode_str(&v.replace('+', " ")).decode_utf8_lossy().to_string())
                    .unwrap_or_default();
                match self.list(&rel) {
                    Ok(entries) => reply(StatusCode::OK, "application/json", json!({ "entries": entries }).to_string()),
                    Err(e) => reply(StatusCode::FORBIDDEN, "application/json", json!({ "error": e }).to_string()),
                }
            }
            _ => reply(StatusCode::NOT_FOUND, "text/plain", "Not found".into()),
        }
    }

    /// A folder's contents, folders first, for the built-in browser.
    fn list(&self, rel: &str) -> Result<Vec<Entry>, String> {
        let rel = clean_rel(rel).ok_or("That folder isn't in the share.")?;
        let dir = self.root.join(&rel);
        if !inside(&self.root, &dir) || !dir.is_dir() {
            return Err("That folder isn't in the share.".into());
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let path = e.path();
            // Links aren't listed, as over WebDAV (where dav-server hides them).
            if e.file_type().is_ok_and(|t| t.is_symlink()) || !inside(&self.root, &path) {
                continue;
            }
            let Ok(m) = std::fs::metadata(&path) else { continue };
            let modified = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            out.push(Entry { name: e.file_name().to_string_lossy().to_string(), dir: m.is_dir(), size: if m.is_dir() { 0 } else { m.len() }, modified });
        }
        out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }
}

/// A path inside the share as the browser sends it ("Photos/2026"): parts
/// only, never `..`, never absolute.
fn clean_rel(rel: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rel.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." || part.contains(':') {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

// ------------------------------------------------------------------ client

/// What a path segment keeps as is in a URL.
const SEGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}').add(b'/').add(b'\\');

/// `/<name>/<rel>` with each part of `rel` encoded.
fn dav_url(name: &str, rel: &str) -> String {
    let mut url = format!("/{name}");
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        url.push('/');
        url.extend(utf8_percent_encode(part, SEGMENT));
    }
    url
}

/// A reply's body: as it comes, or put back together from chunks
/// (`Transfer-Encoding: chunked`, which the server uses when it doesn't say
/// the size up front).
pub struct BodyReader {
    r: BufReader<TcpStream>,
    chunked: bool,
    /// Bytes left in the current chunk.
    left: u64,
    done: bool,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.r.read(buf);
        }
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            if line.trim().is_empty() {
                line.clear();
                self.r.read_line(&mut line)?; // the CRLF that ends the last chunk
            }
            let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let want = buf.len().min(self.left as usize);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "cut off"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Sends one request to the near end of the tunnel and reads the reply's
/// status and headers; the body is left in the reader.
fn request(port: u16, method: &str, url: &str, headers: &str, timeout: Duration) -> Result<(u16, u64, BodyReader), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("The tunnel isn't up: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.write_all(format!("{method} {url} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    read_head(BufReader::new(s))
}

fn read_head(mut r: BufReader<TcpStream>) -> Result<(u16, u64, BodyReader), String> {
    let mut line = String::new();
    r.read_line(&mut line).map_err(|_| "The host didn't answer.".to_string())?;
    let status = line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).ok_or("The host sent a bad reply.")?;
    let (mut len, mut chunked) = (u64::MAX, false);
    loop {
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(u64::MAX);
            } else if k.trim().eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    Ok((status, len, BodyReader { r, chunked, left: 0, done: false }))
}

fn json_get(port: u16, url: &str) -> Result<serde_json::Value, String> {
    let (status, _, mut r) = request(port, "GET", url, "", Duration::from_secs(20))?;
    let mut body = String::new();
    r.read_to_string(&mut body).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "The host sent a bad reply.".to_string())?;
    if status != 200 {
        return Err(v["error"].as_str().unwrap_or("The host refused.").to_string());
    }
    Ok(v)
}

/// The mount's name and whether changes are allowed.
pub fn info(port: u16) -> Result<Info, String> {
    serde_json::from_value(json_get(port, "/.kittyconnect/info")?).map_err(|e| e.to_string())
}

/// A folder's contents (for the built-in browser).
pub fn list(port: u16, rel: &str) -> Result<Vec<Entry>, String> {
    let url = format!("/.kittyconnect/list?path={}", utf8_percent_encode(rel, SEGMENT));
    serde_json::from_value(json_get(port, &url)?["entries"].clone()).map_err(|e| e.to_string())
}

/// Downloads a file of the mount into `dest` (a folder), reporting progress.
/// Returns where it was saved.
pub fn download(port: u16, name: &str, rel: &str, dest: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, String> {
    let file_name = rel.rsplit('/').next().unwrap_or("file");
    let safe = crate::share::safe_relative(file_name).ok_or("The file name isn't safe to save.")?;
    std::fs::create_dir_all(dest).map_err(|e| format!("Couldn't create {}: {e}", dest.display()))?;
    let target = crate::share::unique(&dest.join(safe));
    let (status, total, mut r) = request(port, "GET", &dav_url(name, rel), "", Duration::from_secs(60))?;
    if status != 200 {
        return Err(format!("The host refused ({status})."));
    }
    let part = target.with_file_name(format!("{}.part", target.file_name().unwrap_or_default().to_string_lossy()));
    let mut out = std::fs::File::create(&part).map_err(|e| format!("Couldn't save to {}: {e}", part.display()))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err("Cancelled.".to_string());
        }
        match r.read(&mut buf) {
            Ok(0) => break if total == u64::MAX || done == total { Ok(()) } else { Err("The connection dropped before the file finished.".into()) },
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(format!("Couldn't write the file: {e}"));
                }
                done += n as u64;
                progress(done, total);
            }
            Err(e) => break Err(format!("The connection dropped: {e}")),
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

/// Uploads a file into a folder of the mount (only when the host allows changes).
pub fn upload(port: u16, name: &str, rel_dir: &str, file: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<(), String> {
    let file_name = file.file_name().map(|n| n.to_string_lossy().to_string()).ok_or("No file name.")?;
    let mut f = std::fs::File::open(file).map_err(|e| format!("Couldn't open {}: {e}", file.display()))?;
    let total = f.metadata().map(|m| m.len()).unwrap_or(0);
    let rel = if rel_dir.is_empty() { file_name } else { format!("{}/{file_name}", rel_dir.trim_end_matches('/')) };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("The tunnel isn't up: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    s.write_all(
        format!("PUT {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {total}\r\n\r\n", dav_url(name, &rel)).as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        let n = f.read(&mut buf).map_err(|e| format!("Couldn't read the file: {e}"))?;
        if n == 0 {
            break;
        }
        s.write_all(&buf[..n]).map_err(|e| format!("The connection dropped: {e}"))?;
        done += n as u64;
        progress(done, total);
    }
    let (status, _, _) = read_head(BufReader::new(s))?;
    match status {
        200..=299 => Ok(()),
        403 | 405 => Err("The host only allows reading this folder.".into()),
        s => Err(format!("The host refused ({s}).")),
    }
}

// --------------------------------------------------- mounting on this PC

/// Where a mount shows up on this PC.
#[derive(Clone, Debug, PartialEq)]
pub enum Mounted {
    /// Windows: a drive letter.
    Drive(char),
    /// macOS: a volume (in Finder's sidebar).
    Volume(PathBuf),
}

impl Mounted {
    pub fn label(&self) -> String {
        match self {
            Mounted::Drive(l) => format!("{l}:"),
            Mounted::Volume(p) => p.display().to_string(),
        }
    }
}

fn mounts_list() -> PathBuf {
    crate::portable::data_dir().join("run").join("mounts.txt")
}

/// Notes a mount, so one left behind by a crash is undone at the next start.
fn remember(m: &Mounted, add: bool) {
    let line = match m {
        Mounted::Drive(l) => format!("drive {l}"),
        Mounted::Volume(p) => format!("volume {}", p.display()),
    };
    let path = mounts_list();
    let mut lines: Vec<String> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(str::to_string).filter(|l| *l != line).collect();
    if add {
        lines.push(line);
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, lines.join("\n"));
}

#[cfg(windows)]
fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
}

/// The first two lines of what a command said, for an error message.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn said(o: &std::process::Output) -> String {
    let text = format!("{}\n{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout));
    text.lines().map(str::trim).filter(|l| !l.is_empty()).take(2).collect::<Vec<_>>().join(" ")
}

/// Mounts the share reached through local `port`: a drive letter on Windows,
/// a volume on macOS. Blocks for a few seconds: call it off the UI thread.
pub fn mount(port: u16, name: &str) -> Result<Mounted, String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
        let used = unsafe { GetLogicalDrives() };
        let letter = ('D'..='Z').rev().find(|l| used & (1 << (*l as u32 - 'A' as u32)) == 0).ok_or("Every drive letter is in use.")?;
        let url = format!("http://127.0.0.1:{port}/{name}");
        let o = quiet(std::process::Command::new("net").args(["use", &format!("{letter}:"), &url, "/persistent:no"]))
            .output()
            .map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(format!(
                "Windows couldn't map it ({}). Its WebDAV service (WebClient) may be turned off. You can still browse it here.",
                said(&o)
            ));
        }
        let m = Mounted::Drive(letter);
        remember(&m, true);
        Ok(m)
    }
    #[cfg(target_os = "macos")]
    {
        let url = format!("http://127.0.0.1:{port}/{name}/");
        let o = std::process::Command::new("osascript").args(["-e", &format!("mount volume \"{url}\"")]).output().map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(format!("macOS couldn't mount it ({}). You can still browse it here.", said(&o)));
        }
        let point = mount_point(&url).ok_or("macOS mounted it, but it isn't listed. Look in Finder's sidebar.")?;
        let m = Mounted::Volume(point);
        remember(&m, true);
        Ok(m)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (port, name);
        Err("Mounting as a drive is for Windows and macOS. You can still browse it here.".into())
    }
}

/// Where macOS mounted `url`, from `mount`'s list ("<url> on <point> (webdav, …)").
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mount_point(url: &str) -> Option<PathBuf> {
    let o = std::process::Command::new("mount").output().ok()?;
    parse_mount_point(&String::from_utf8_lossy(&o.stdout), url)
}

fn parse_mount_point(listing: &str, url: &str) -> Option<PathBuf> {
    let base = url.trim_end_matches('/');
    listing.lines().find_map(|l| {
        let (src, rest) = l.split_once(" on ")?;
        if src.trim_end_matches('/') != base {
            return None;
        }
        Some(PathBuf::from(rest.rsplit_once(" (").map_or(rest, |(p, _)| p)))
    })
}

/// Undoes a mount. Blocks briefly: call it off the UI thread, before the
/// tunnel closes (a drive whose server is gone makes Explorer and Finder wait).
pub fn unmount(m: &Mounted) {
    match m {
        #[cfg(windows)]
        Mounted::Drive(l) => {
            let _ = quiet(std::process::Command::new("net").args(["use", &format!("{l}:"), "/delete", "/y"])).output();
        }
        #[cfg(not(windows))]
        Mounted::Drive(_) => {}
        Mounted::Volume(p) => {
            let ok = std::process::Command::new("diskutil").args(["unmount", "force"]).arg(p).output().is_ok_and(|o| o.status.success());
            if !ok {
                let _ = std::process::Command::new("umount").arg("-f").arg(p).output();
            }
        }
    }
    remember(m, false);
}

/// Shows it in Explorer or Finder.
pub fn open(m: &Mounted) {
    match m {
        Mounted::Drive(l) => crate::open_in_shell(format!("{l}:\\")),
        Mounted::Volume(p) => crate::open_in_shell(p),
    }
}

/// Undoes mounts a run that crashed left behind (only ones KittyConnect made,
/// and only while they still point at this PC).
pub fn sweep_leftovers() {
    let Ok(text) = std::fs::read_to_string(mounts_list()) else { return };
    for line in text.lines() {
        if let Some(l) = line.strip_prefix("drive ").and_then(|l| l.chars().next()) {
            #[cfg(windows)]
            {
                let o = quiet(std::process::Command::new("net").args(["use", &format!("{l}:")])).output();
                if o.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("127.0.0.1")) {
                    let _ = quiet(std::process::Command::new("net").args(["use", &format!("{l}:"), "/delete", "/y"])).output();
                }
            }
            let _ = l;
        } else if let Some(p) = line.strip_prefix("volume ") {
            let listing = std::process::Command::new("mount").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
            if listing.lines().any(|l| l.starts_with("http://127.0.0.1:") && l.contains(&format!(" on {p} ("))) {
                let _ = std::process::Command::new("diskutil").args(["unmount", "force", p]).output();
            }
        }
    }
    let _ = std::fs::remove_file(mounts_list());
}

// ------------------------------------------------ Windows' 50 MB limit

#[cfg(windows)]
const WEBCLIENT_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters";

/// The largest file Windows' WebDAV will open (50 MB unless raised). None
/// where it doesn't apply (macOS) or can't be read.
pub fn size_limit() -> Option<u64> {
    #[cfg(windows)]
    {
        let o = quiet(std::process::Command::new("reg").args(["query", WEBCLIENT_KEY, "/v", "FileSizeLimitInBytes"])).output().ok()?;
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        let hex = text.split_whitespace().find(|w| w.starts_with("0x"))?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Run by KittyConnect itself as administrator (`--kc-webdav-big-files`):
/// raises the limit to Windows' most (4 GB) and restarts its WebDAV service.
pub fn raise_limit_elevated() -> i32 {
    #[cfg(windows)]
    {
        let ok = quiet(std::process::Command::new("reg").args([
            "add",
            WEBCLIENT_KEY,
            "/v",
            "FileSizeLimitInBytes",
            "/t",
            "REG_DWORD",
            "/d",
            "4294967295",
            "/f",
        ]))
        .status()
        .is_ok_and(|s| s.success());
        let _ = quiet(std::process::Command::new("net").args(["stop", "WebClient"])).status();
        let _ = quiet(std::process::Command::new("net").args(["start", "WebClient"])).status();
        if ok {
            0
        } else {
            1
        }
    }
    #[cfg(not(windows))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kc-mount-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Shared Stuff/Photos")).unwrap();
        std::fs::write(d.join("Shared Stuff/notes.txt"), b"hello").unwrap();
        std::fs::write(d.join("Shared Stuff/Photos/a.jpg"), vec![7u8; 300_000]).unwrap();
        std::fs::write(d.join("secret.txt"), b"not shared").unwrap();
        d
    }

    /// A raw HTTP exchange with the server: (status, body).
    fn http(port: u16, method: &str, url: &str, headers: &str, body: &[u8]) -> (u16, String) {
        let (s, _, b) = http_full(port, method, url, headers, body);
        (s, b)
    }

    /// The same, with the reply's headers.
    fn http_full(port: u16, method: &str, url: &str, headers: &str, body: &[u8]) -> (u16, String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let h = format!("{method} {url} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n", body.len());
        s.write_all(h.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (status, head.to_string(), body.to_string())
    }

    #[test]
    fn names_are_url_safe() {
        assert_eq!(url_name(Path::new("/x/Shared Stuff")), "Shared-Stuff");
        assert_eq!(url_name(Path::new("/x/Fotos 2026 (alt)!")), "Fotos-2026-alt");
        assert_eq!(url_name(Path::new("/x/...")), "Shared");
        assert_eq!(url_name(Path::new("/x/.kittyconnect")), "kittyconnect", "never the server's own /.kittyconnect/");
    }

    #[test]
    fn paths_stay_in_the_share() {
        assert_eq!(clean_rel("Photos/2026"), Some(PathBuf::from("Photos").join("2026")));
        assert_eq!(clean_rel(""), Some(PathBuf::new()));
        assert_eq!(clean_rel("../secret.txt"), None);
        assert_eq!(clean_rel("C:/Windows"), None);
        let d = temp("inside");
        let root = std::fs::canonicalize(d.join("Shared Stuff")).unwrap();
        assert!(inside(&root, &root.join("notes.txt")));
        assert!(inside(&root, &root.join("new folder")), "not there yet, in a folder that is");
        assert!(!inside(&root, &root.join("..").join("secret.txt")));
        assert!(!inside(&root, &root.join("nope").join("..").join("..").join("secret.txt")));
    }

    #[test]
    fn mac_mount_points_are_found() {
        let listing = "/dev/disk3s1 on / (apfs, local)\nhttp://127.0.0.1:51234/Shared-Stuff/ on /Volumes/Shared-Stuff (webdav, nodev, noexec, nosuid, mounted by sam)\n";
        assert_eq!(parse_mount_point(listing, "http://127.0.0.1:51234/Shared-Stuff/"), Some(PathBuf::from("/Volumes/Shared-Stuff")));
        assert_eq!(parse_mount_point(listing, "http://127.0.0.1:1/Other/"), None);
    }

    /// Read only: listing and reading work; every kind of change is refused.
    #[test]
    fn a_read_only_mount() {
        let d = temp("ro");
        let srv = MountServer::start(&d.join("Shared Stuff"), false).unwrap();
        let (p, name) = (srv.port, srv.name.clone());
        assert_eq!(name, "Shared-Stuff");
        // What Windows and Finder ask first.
        let (s, _) = http(p, "OPTIONS", "/", "", b"");
        assert_eq!(s, 200);
        let (s, body) = http(p, "PROPFIND", &format!("/{name}/"), "Depth: 1\r\n", b"");
        assert_eq!(s, 207);
        assert!(body.contains("notes.txt") && body.contains("Photos"), "{body}");
        assert!(!body.contains("secret"), "only the shared folder");
        let (s, body) = http(p, "GET", &format!("/{name}/notes.txt"), "", b"");
        assert_eq!((s, body.as_str()), (200, "hello"));
        // No changes of any kind.
        assert!(http(p, "PUT", &format!("/{name}/new.txt"), "", b"x").0 >= 400);
        assert!(http(p, "DELETE", &format!("/{name}/notes.txt"), "", b"").0 >= 400);
        assert!(http(p, "MKCOL", &format!("/{name}/New"), "", b"").0 >= 400);
        assert!(http(p, "MOVE", &format!("/{name}/notes.txt"), &format!("Destination: /{name}/moved.txt\r\n"), b"").0 >= 400);
        assert!(d.join("Shared Stuff/notes.txt").exists() && !d.join("Shared Stuff/new.txt").exists());
        // Out of the folder: refused.
        assert!(http(p, "GET", &format!("/{name}/../secret.txt"), "", b"").0 >= 400);
        assert!(http(p, "GET", &format!("/{name}/%2e%2e/secret.txt"), "", b"").0 >= 400);
        assert_eq!(http(p, "GET", "/secret.txt", "", b"").0, 404);
        // The built-in browser's side.
        assert_eq!(info(p).unwrap(), Info { name: name.clone(), title: "Shared Stuff".into(), write: false });
        let l = list(p, "").unwrap();
        assert_eq!(l.iter().map(|e| (e.name.as_str(), e.dir)).collect::<Vec<_>>(), vec![("Photos", true), ("notes.txt", false)]);
        assert_eq!(list(p, "Photos").unwrap()[0].size, 300_000);
        assert!(list(p, "../").is_err());
        let cancel = AtomicBool::new(false);
        let out = d.join("got");
        let mut last = 0;
        let saved = download(p, &name, "Photos/a.jpg", &out, &cancel, &mut |done, _| last = done).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap().len(), 300_000);
        assert_eq!(last, 300_000);
        let up = d.join("up.txt");
        std::fs::write(&up, b"upload").unwrap();
        assert!(upload(p, &name, "", &up, &cancel, &mut |_, _| {}).is_err());
        drop(srv);
        let closed = (0..30).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            TcpStream::connect(("127.0.0.1", p)).is_err()
        });
        assert!(closed, "stopped with the share");
    }

    /// Read and write: files and folders can be added, changed, moved and deleted, inside the folder only.
    #[test]
    fn a_read_write_mount() {
        let d = temp("rw");
        let srv = MountServer::start(&d.join("Shared Stuff"), true).unwrap();
        let (p, name) = (srv.port, srv.name.clone());
        let (_, opts) = (0, http(p, "OPTIONS", &format!("/{name}/"), "", b""));
        assert_eq!(opts.0, 200);
        assert_eq!(http(p, "PUT", &format!("/{name}/new.txt"), "", b"written").0 / 100, 2);
        assert_eq!(std::fs::read(d.join("Shared Stuff/new.txt")).unwrap(), b"written");
        // What Explorer sends after copying a file in: its timestamps. It
        // must hear "OK", or Windows reports the copy as failed.
        let patch = br#"<?xml version="1.0" encoding="utf-8" ?><D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32CreationTime>Thu, 12 Nov 2026 10:00:00 GMT</Z:Win32CreationTime><Z:Win32LastModifiedTime>Thu, 12 Nov 2026 10:00:00 GMT</Z:Win32LastModifiedTime><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes></D:prop></D:set></D:propertyupdate>"#;
        let (s, body) = http(p, "PROPPATCH", &format!("/{name}/new.txt"), "Content-Type: text/xml\r\n", patch);
        assert_eq!(s, 207);
        assert!(body.contains("200 OK") && !body.contains("424") && !body.contains("403"), "{body}");
        assert_eq!(http(p, "MKCOL", &format!("/{name}/New%20Folder"), "", b"").0, 201);
        assert!(d.join("Shared Stuff/New Folder").is_dir());
        assert_eq!(http(p, "MOVE", &format!("/{name}/new.txt"), &format!("Destination: /{name}/New%20Folder/moved.txt\r\n"), b"").0 / 100, 2);
        assert!(d.join("Shared Stuff/New Folder/moved.txt").exists());
        // Finder and Office lock before writing; a locked file is kept from
        // anyone else until the lock's holder is done.
        let lock = b"<?xml version=\"1.0\"?><D:lockinfo xmlns:D=\"DAV:\"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>";
        let (s, head, _) = http_full(p, "LOCK", &format!("/{name}/notes.txt"), "Timeout: Second-600\r\n", lock);
        assert_eq!(s, 200);
        let token = head
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("lock-token")).map(|(_, v)| v.trim().to_string()))
            .expect("a lock token");
        assert_eq!(http(p, "DELETE", &format!("/{name}/notes.txt"), "", b"").0, 423, "someone else can't");
        assert!(d.join("Shared Stuff/notes.txt").exists());
        assert_eq!(http(p, "DELETE", &format!("/{name}/notes.txt"), &format!("If: ({token})\r\n"), b"").0 / 100, 2, "the holder can");
        assert!(!d.join("Shared Stuff/notes.txt").exists());
        // Nothing outside the folder, even with write access.
        assert!(http(p, "PUT", &format!("/{name}/../escaped.txt"), "", b"x").0 >= 400);
        assert!(http(p, "MOVE", &format!("/{name}/Photos"), &format!("Destination: /{name}/../Photos\r\n"), b"").0 >= 400);
        assert!(!d.join("escaped.txt").exists() && d.join("Shared Stuff/Photos").is_dir());
        // The built-in browser can upload.
        let up = d.join("up.txt");
        std::fs::write(&up, vec![1u8; 100_000]).unwrap();
        let cancel = AtomicBool::new(false);
        upload(p, &name, "Photos", &up, &cancel, &mut |_, _| {}).unwrap();
        assert_eq!(std::fs::read(d.join("Shared Stuff/Photos/up.txt")).unwrap().len(), 100_000);
        assert!(info(p).unwrap().write);
    }

    /// A link inside the share that leads out of it is neither listed nor followed.
    #[cfg(unix)]
    #[test]
    fn links_out_of_the_share_lead_nowhere() {
        let d = temp("link");
        std::os::unix::fs::symlink(d.join("secret.txt"), d.join("Shared Stuff/sneaky.txt")).unwrap();
        std::os::unix::fs::symlink(&d, d.join("Shared Stuff/everything")).unwrap();
        std::os::unix::fs::symlink(d.join("Shared Stuff/Photos"), d.join("Shared Stuff/photos-again")).unwrap();
        for write in [false, true] {
            let srv = MountServer::start(&d.join("Shared Stuff"), write).unwrap();
            let (p, name) = (srv.port, srv.name.clone());
            let (_, body) = http(p, "PROPFIND", &format!("/{name}/"), "Depth: 1\r\n", b"");
            assert!(!body.contains("sneaky") && !body.contains("everything"), "{body}");
            // A link that stays inside isn't listed either (dav-server hides
            // links), but can be followed.
            assert_eq!(http(p, "GET", &format!("/{name}/photos-again/a.jpg"), "", b"").0, 200);
            assert!(http(p, "GET", &format!("/{name}/sneaky.txt"), "", b"").0 >= 400);
            assert!(http(p, "GET", &format!("/{name}/everything/secret.txt"), "", b"").0 >= 400);
            assert!(http(p, "PUT", &format!("/{name}/everything/planted.txt"), "", b"x").0 >= 400);
            assert!(!list(p, "").unwrap().iter().any(|e| e.name == "sneaky.txt" || e.name == "everything"));
            assert!(list(p, "everything").is_err());
        }
        assert!(!d.join("planted.txt").exists());
        assert_eq!(std::fs::read(d.join("secret.txt")).unwrap(), b"not shared");
    }

    /// The real macOS WebDAV file system (what Finder uses) against the
    /// server: mount, read, write, unmount. Needs a Mac; run with --ignored.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn macos_mounts_it() {
        let d = temp("macos");
        let srv = MountServer::start(&d.join("Shared Stuff"), true).unwrap();
        let point = d.join("mnt");
        std::fs::create_dir_all(&point).unwrap();
        let url = format!("http://127.0.0.1:{}/{}/", srv.port, srv.name);
        let o = std::process::Command::new("mount_webdav").args(["-S", "-v", "KittyConnectTest", &url]).arg(&point).output().unwrap();
        assert!(o.status.success(), "mount_webdav: {}", said(&o));
        let result = std::panic::catch_unwind(|| {
            assert_eq!(std::fs::read(point.join("notes.txt")).unwrap(), b"hello");
            assert_eq!(std::fs::read(point.join("Photos/a.jpg")).unwrap().len(), 300_000);
            std::fs::write(point.join("from-finder.txt"), b"hi from macOS").unwrap();
            std::fs::create_dir(point.join("Made on Mac")).unwrap();
        });
        let _ = std::process::Command::new("umount").arg(&point).output();
        result.unwrap();
        assert_eq!(std::fs::read(d.join("Shared Stuff/from-finder.txt")).unwrap(), b"hi from macOS");
        assert!(d.join("Shared Stuff/Made on Mac").is_dir());
    }
}

/// Runs a read-write mount for a minute, for poking at by hand (litmus, a
/// WebDAV client): `KC_MOUNT_DIR=… cargo test serve_for_a_minute -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn serve_for_a_minute() {
    let dir = PathBuf::from(std::env::var("KC_MOUNT_DIR").expect("KC_MOUNT_DIR"));
    let srv = MountServer::start(&dir, true).unwrap();
    println!("SERVING http://127.0.0.1:{}/{}/", srv.port, srv.name);
    std::thread::sleep(Duration::from_secs(60));
}
