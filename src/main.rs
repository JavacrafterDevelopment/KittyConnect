// No console window in release builds; debug builds keep one for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The state snapshot is one big json! literal.
#![recursion_limit = "256"]

mod app;
mod beacon;
// Native screen share; only built for Windows and macOS.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod capture;
mod code;
// The end-to-end encryption check (tools/encryption-check/run.sh).
#[cfg(any(all(test, target_os = "linux"), feature = "e2e"))]
mod e2e;
// Windows Firewall only; on other systems its helpers are used by its tests.
#[cfg_attr(not(windows), allow(dead_code))]
mod firewall;
mod hub;
mod hubstore;
mod icon;
mod lan;
mod loopback;
mod mount;
mod peers;
mod portable;
mod share;
mod tailcat;
mod voip;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
#[cfg(target_os = "macos")]
use tao::platform::macos::WindowBuilderExtMacOS;
#[cfg(windows)]
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Icon, ResizeDirection, Window, WindowBuilder};
use wry::{DragDropEvent, WebContext, WebView, WebViewBuilder};
#[cfg(windows)]
use wry::WebViewBuilderExtWindows;

use code::Kind;
use peers::{Heard, PortCheck, Peers, Report};
use portable::data_dir;
use share::{Library, RecvEvent, Remote, Server, ShareEvent};
use tailcat::{Link, Proc, Role, TcEvent};
use voip::{Voip, VoipEvent};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const LOG_LINES: usize = 200;

pub enum UserEvent {
    Ipc(String),
    /// A menu bar item (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Menu(String),
    Tc(Role, u64, TcEvent),
    Share(ShareEvent),
    Recv(RecvEvent),
    Dropped(Vec<PathBuf>),
    Tick,
    Voip(u64, VoipEvent),
    /// A 30-second liveness ping came back (or didn't).
    Ping(PingTarget, Result<tailcat::Pong, String>),
    /// Someone connected to our file share (`share`) or one of our open
    /// ports checked in or said goodbye.
    Heard { share: bool, heard: Heard },
    /// A Minecraft host's beacon said which LAN worlds it has open.
    Lan(u64, Result<Vec<lan::World>, String>),
    /// "Ask for network permission again" finished (how many programs), or didn't.
    Firewall(Result<usize, String>),
    /// Whether something is listening on each port checked (see `App::probe`).
    Probed(Vec<(u16, bool)>),
    /// A joined mount, by key: what the host said about it.
    MountInfo(u64, Result<mount::Info, String>),
    /// A joined mount was mounted (Some) or unmounted (None), or couldn't be.
    MountDone(u64, Result<Option<mount::Mounted>, String>),
    /// A folder's contents for the built-in browser: (key, path, entries).
    MountList(u64, String, Result<Vec<mount::Entry>, String>),
    /// A download or upload of a joined mount: (key, transfer id, what happened).
    MountXfer(u64, u64, Xfer),
    /// The largest file Windows' WebDAV opens (None: no limit here, or unknown).
    WebdavLimit(Option<u64>),
    /// Our hub connection (by generation) heard something.
    Hub(u64, hub::HubEvent),
    /// A saved hub's key was made (for the Hub with this tag): the hub, the
    /// key's address or why not, and whether to open it now.
    HubKey(u64, hubstore::Saved, Result<String, String>, bool),
    /// Scan: one code of a saved hub's address book (for that scan of that
    /// hub), and what answered there.
    HubScan(u64, String, u64, hubstore::Entry, Result<hub::Peeked, String>),
}

pub enum Xfer {
    Progress(u64, u64),
    Done(Result<Option<PathBuf>, String>),
}

#[derive(Clone, Copy)]
pub enum PingTarget {
    /// The file share we're receiving from, by its local tunnel port.
    Recv(u16),
    /// A joined port set, by key.
    Join(u64),
}

/// Bytes per second, smoothed so the number doesn't jump around.
#[derive(Clone)]
struct Speed {
    at: Instant,
    done: u64,
    bps: f64,
}

impl Speed {
    fn new(done: u64) -> Speed {
        Speed { at: Instant::now(), done, bps: 0.0 }
    }

    fn update(&mut self, done: u64) {
        let dt = self.at.elapsed().as_secs_f64();
        if dt < 0.15 {
            return;
        }
        let rate = done.saturating_sub(self.done) as f64 / dt;
        self.bps = if self.bps == 0.0 { rate } else { self.bps * 0.6 + rate * 0.4 };
        self.at = Instant::now();
        self.done = done;
    }
}

struct Sending {
    name: String,
    done: u64,
    total: u64,
    speed: Speed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Off,
    Starting,
    Live,
    Error,
}

impl St {
    fn name(self) -> &'static str {
        match self {
            St::Off => "off",
            St::Starting => "starting",
            St::Live => "live",
            St::Error => "error",
        }
    }
}

#[derive(Default)]
pub(crate) struct Log(VecDeque<String>);

impl Log {
    pub(crate) fn push(&mut self, line: impl Into<String>) {
        if self.0.len() >= LOG_LINES {
            self.0.pop_front();
        }
        self.0.push_back(line.into());
    }
    pub(crate) fn json(&self) -> Value {
        json!(self.0)
    }
}

/// Handles one TailCat output line for a tunnel: updates its link (relay or
/// direct) and keeps the line in the visible log unless it's verbose chatter.
fn tc_line(log: &mut Log, link: &mut Link, line: String) {
    link.observe(&line);
    if !tailcat::is_verbose(&line) || tailcat::keep_verbose(&line) {
        log.push(line);
    }
}

/// Sharing files: our file server plus `tailcat serve <its port>`.
struct ShareHost {
    st: St,
    error: String,
    lib: Library,
    server: Option<Server>,
    proc: Option<Proc>,
    addr: Option<String>,
    /// Downloads in progress, by connection.
    sending: BTreeMap<u64, Sending>,
    sent: u64,
    link: Link,
    /// Everyone receiving from us, with how each is connected.
    peers: Peers,
    log: Log,
}

#[derive(Clone)]
struct FileState {
    st: &'static str,
    done: u64,
    total: u64,
    speed: Speed,
    path: Option<PathBuf>,
    error: String,
}

/// Receiving files: `tailcat forward <addr> <local>:<remote>` and our client.
struct Recv {
    st: St,
    error: String,
    proc: Option<Proc>,
    port: u16,
    cancel: Arc<AtomicBool>,
    jobs: Option<Sender<share::Job>>,
    files: Vec<Remote>,
    states: BTreeMap<u64, FileState>,
    inbox: PathBuf,
    link: Link,
    /// The `tailcat forward` arguments, to reconnect with ("Retry direct").
    args: Vec<String>,
    /// Who we are to the sharer, and who they are to us.
    peer: u64,
    addr: String,
    host_name: Option<String>,
    log: Log,
}

/// One opened port (or set of ports): `tailcat serve <ports>`. Each has its own
/// TailCat process and code, and is listed until stopped.
struct PortHost {
    key: u64,
    st: St,
    error: String,
    warn: String,
    proc: Option<Proc>,
    ports: Vec<u16>,
    addr: Option<String>,
    link: Link,
    /// Everyone who joined this one, with how each is connected.
    peers: Peers,
    /// A one-click game server ("minecraft"), or None for a manual port.
    game: Option<String>,
    /// A shared folder (Mounts tab): its WebDAV server is the port served.
    mount: Option<MountShare>,
    /// Its ports have been checked for a server at least once.
    probed: bool,
    /// For Minecraft: what friends see, once it's showing.
    lan_note: String,
    log: Log,
}

/// A folder shared from the Mounts tab.
struct MountShare {
    dir: PathBuf,
    write: bool,
    /// Held so the server runs while the share is on; dropping it stops it.
    #[allow(dead_code)]
    server: mount::MountServer,
}

/// A joined mount: what the host said, and how it's open on this PC.
#[derive(Default)]
struct MountJoin {
    info: Option<mount::Info>,
    asking: bool,
    asked_at: Option<Instant>,
    mounted: Option<mount::Mounted>,
    /// "mounting", "unmounting", or "".
    busy: &'static str,
    error: String,
    browse: Option<Browse>,
    transfers: Vec<Transfer>,
}

/// The built-in browser's place in a joined mount.
struct Browse {
    path: String,
    entries: Vec<mount::Entry>,
    loading: bool,
    error: String,
}

struct Transfer {
    id: u64,
    name: String,
    up: bool,
    done: u64,
    total: u64,
    /// "going", "done" or "error".
    st: &'static str,
    error: String,
    path: Option<PathBuf>,
    speed: Speed,
    cancel: Arc<AtomicBool>,
}

impl PortHost {
    /// The code is closing: its ports check as wrong, and joiners hear of no
    /// more LAN worlds on it.
    fn unserve(&mut self) {
        if let Some(addr) = &self.addr {
            peers::unserve(addr);
            if self.game.is_some() {
                lan::unserve(addr);
            }
        }
    }
}

impl Drop for PortHost {
    fn drop(&mut self) {
        self.unserve();
    }
}

/// One joined port set: `tailcat forward <addr> <local>:<remote>…`, plus a
/// mapping to the host's beacon for the 30-second check.
struct PortJoin {
    key: u64,
    st: St,
    error: String,
    proc: Option<Proc>,
    maps: Vec<(u16, u16)>,
    lan: bool,
    started: Instant,
    beacon: Option<u16>,
    link: Link,
    /// The page has been told it can't reach the host (so it's told once).
    told_unreachable: bool,
    /// The host said its code opened every port we forward (or it's an older
    /// version that can't say). Until then it isn't "Connected".
    ports_checked: bool,
    /// The `tailcat forward` arguments, to reconnect with ("Retry direct").
    args: Vec<String>,
    peer: u64,
    addr: String,
    host_name: Option<String>,
    /// A one-click game server, from the code.
    game: Option<String>,
    /// Minecraft: the host's LAN worlds, asked for every few seconds.
    worlds: Vec<lan::World>,
    lan_next: Instant,
    lan_asking: bool,
    /// A joined mount (Mounts tab).
    mount: Option<MountJoin>,
    /// "Share on my network": the picked ports on every address of this PC.
    relays: Vec<loopback::LanRelay>,
    log: Log,
}

/// Where a join comes from: the forms differ in what codes they take.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinFrom {
    Manual,
    Game,
    Mount,
}

/// Something open on this PC that the hub can invite people to.
struct Invitable {
    /// What the page calls it: "voip", "share" or "host:<key>".
    what: String,
    kind: &'static str,
    title: String,
    code: String,
}

/// The one-click games' names, for invites and the Connections list (the
/// page has the same list).
fn game_name(id: &str) -> &str {
    match id {
        "minecraft" => "Minecraft: Java Edition",
        "terraria" => "Terraria",
        "starbound" => "Starbound",
        "vintagestory" => "Vintage Story",
        other => other,
    }
}

/// "Port 25565" or "Ports 25565, 8080".
fn ports_title(ports: &[u16]) -> String {
    format!("Port{} {}", if ports.len() == 1 { "" } else { "s" }, join_ports(ports, ", "))
}

fn join_ports(ports: &[u16], sep: &str) -> String {
    ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(sep)
}

/// A hub code pasted somewhere else.
const HUB_CODE_HERE: &str = "That's a hub code. Paste it in the Hub tab.";

/// What one check of a joined port came to.
#[derive(Debug, PartialEq, Eq)]
enum Checked {
    Nothing,
    Connected,
    /// The host stopped answering: it closed the port or went offline.
    Gone,
    /// The host said the port typed isn't open on its code.
    WrongPorts,
}

/// A joined port that hasn't heard from the host in this long says so (it
/// keeps trying).
const UNREACHED: Duration = Duration::from_secs(30);

impl PortJoin {
    /// The host has answered through the tunnel and said the ports we typed
    /// are open on its code. TailCat listens on this PC before it has even
    /// tried to reach anyone, and a tunnel that's up (even a direct one) only
    /// proves the code is right, not the port number: TailCat on the host
    /// silently drops a port it wasn't told to open. Without a beacon to ask
    /// (the friend's own port is the beacon's), a few quiet seconds have to do.
    fn reached(&self) -> bool {
        match self.beacon {
            Some(_) => self.ports_checked,
            None => self.link.peer_seen || self.link.direct.is_some() || self.started.elapsed() > Duration::from_secs(3),
        }
    }

    /// Takes in what a check through the tunnel said: whether it came back,
    /// and the host's answer about the ports.
    fn checked(&mut self, result: Result<Duration, String>, ports: Option<PortCheck>) -> Checked {
        if result.is_ok() {
            match ports {
                Some(PortCheck::Wrong) => {
                    self.wrong_ports();
                    return Checked::WrongPorts;
                }
                // Open, or an older host that can't check (it never says).
                Some(PortCheck::Open) | None => self.ports_checked = true,
                // Asked too often just now: the next check asks again.
                Some(PortCheck::Busy) => {}
            }
        }
        if self.link.ping_result(result) {
            self.proc = None;
            self.relays.clear();
            self.st = St::Error;
            self.error = "Your friend closed this port or went offline.".into();
            return Checked::Gone;
        }
        if self.promote() {
            Checked::Connected
        } else {
            Checked::Nothing
        }
    }

    /// The host said a port we typed isn't open on its code: stop, so nothing
    /// on this PC listens for it.
    fn wrong_ports(&mut self) {
        let list: Vec<String> = self.maps.iter().map(|(_, r)| r.to_string()).collect();
        self.proc = None;
        self.relays.clear();
        self.st = St::Error;
        self.error = if list.len() == 1 {
            format!("Port {} isn't open on this code. Check the port number with your friend.", list[0])
        } else {
            format!("Not every one of ports {} is open on this code. Check the numbers with your friend.", list.join(", "))
        };
        self.log.push("Portside: the host said this code didn't open that port; stopped");
    }

    /// Joining, and nothing has answered for a while.
    fn unreachable(&self) -> bool {
        self.proc.is_some() && self.st == St::Starting && self.started.elapsed() > UNREACHED
    }

    /// Becomes "Connected" once the host has answered. True if it just did.
    fn promote(&mut self) -> bool {
        if self.st == St::Starting && self.proc.is_some() && self.reached() {
            self.st = St::Live;
            return true;
        }
        false
    }

    fn report(&self) -> Report<'_> {
        // The port numbers go along until the host has said they're open.
        let ports = if self.ports_checked { Vec::new() } else { self.maps.iter().map(|(_, r)| *r).collect() };
        Report { peer: self.peer, addr: &self.addr, path: self.link.path(), rtt: self.link.rtt_ms, ports }
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    share: ShareHost,
    recv: Recv,
    hosts: Vec<PortHost>,
    joins: Vec<PortJoin>,
    /// An error from the last Open/Connect press, shown under its form.
    host_error: String,
    join_error: String,
    next_key: u64,
    voip: Voip,
    hub: hub::Hub,
    /// The tab on screen, so a drop lands where the user expects.
    page: String,
    status: (String, &'static str),
    /// The name picked at startup: shown in rooms, and to whoever's port or
    /// file share we join. Empty until picked.
    name: String,
    /// LAN worlds we're showing in this PC's Minecraft: (local port, MOTD).
    adverts: Vec<(u16, String)>,
    /// Whether a server is listening on each port we opened, as last checked.
    /// Checked off the UI thread: on Windows a refused connection to this PC
    /// takes about a second, which froze the window when done here.
    listening: HashMap<u16, bool>,
    probing: bool,
    probe_at: Instant,
    /// Mounts tab: the folder chosen to share next, and the forms' errors.
    mount_pick: Option<PathBuf>,
    mount_host_error: String,
    mount_join_error: String,
    /// Windows' WebDAV file size limit (None on a Mac, or unknown).
    webdav_limit: Option<u64>,
    next_xfer: u64,
}

fn main() {
    // Run as administrator by "Allow big files" (Mounts): raise Windows'
    // WebDAV file size limit, then exit. Nothing else.
    if std::env::args().any(|a| a == "--kc-webdav-big-files") {
        std::process::exit(mount::raise_limit_elevated());
    }
    // The end-to-end encryption check (tools/encryption-check), only in a
    // build made with `--features e2e`, which no release is. It runs here on
    // the main thread because macOS's event loop must.
    #[cfg(feature = "e2e")]
    if std::env::args().any(|a| a == "--kc-e2e") {
        e2e::run();
        return;
    }
    // A fixed-version WebView2 runtime in a `WebView2` folder beside the exe is
    // used in preference to the system one, for machines without Edge.
    #[cfg(windows)]
    {
        let fixed = portable::app_dir().join("WebView2");
        if fixed.join("msedgewebview2.exe").exists() {
            std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", &fixed);
        }
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    #[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_mut))]
    let mut window_builder = WindowBuilder::new()
        .with_title("Portside")
        .with_inner_size(LogicalSize::new(1060.0, 720.0))
        .with_min_inner_size(LogicalSize::new(760.0, 520.0))
        .with_decorations(false)
        .with_visible(false)
        .with_window_icon(Icon::from_rgba(icon::rgba(64), 64, 64).ok());
    #[cfg(windows)]
    {
        window_builder = window_builder.with_undecorated_shadow(true);
    }
    // A real titled window under a see-through title bar: macOS draws the
    // traffic lights, rounded corners and shadow and resizes from the edges,
    // and the page runs up underneath (the masthead leaves the lights room).
    #[cfg(target_os = "macos")]
    {
        window_builder = window_builder
            .with_decorations(true)
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true)
            .with_traffic_light_inset(tao::dpi::LogicalPosition::new(18.0, 22.0));
    }
    let window = window_builder.build(&event_loop).expect("create window");
    #[cfg(target_os = "macos")]
    let menu = menu_bar(proxy.clone());

    let mut web_context = WebContext::new(Some(data_dir().join("webview")));
    let ipc_proxy = proxy.clone();
    let drop_proxy = proxy.clone();
    // Old room files and image previews from a run that didn't close cleanly.
    let _ = std::fs::remove_dir_all(voip::rooms_dir());
    let _ = std::fs::remove_dir_all(data_dir().join("cache"));
    // The page is served from kc://localhost, which WebView2 loads as
    // https://kc.localhost: a secure origin, which screen capture and the
    // browser's video encoder and decoder require. Nothing here is fetched
    // from the network; the handler answers from memory and the preview cache.
    let html = std::sync::Arc::new(page_html());
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut webview_builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_custom_protocol("kc".into(), move |_id, req| serve_page(&html, req.uri().path()))
        .with_url("kc://localhost/")
        .with_background_color((255, 255, 255, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(req.into_body()));
        })
        .with_drag_drop_handler(move |event| {
            if let DragDropEvent::Drop { paths, .. } = event {
                let _ = drop_proxy.send_event(UserEvent::Dropped(paths));
            }
            true
        });
    #[cfg(windows)]
    {
        webview_builder = webview_builder.with_browser_accelerator_keys(false).with_https_scheme(true);
    }
    let webview = match webview_builder.build(&window) {
        Ok(w) => w,
        Err(e) => {
            missing_webview2(&e.to_string());
            return;
        }
    };

    // Unpack TailCat in the background so the first Start is quick, and end
    // any TailCat an earlier run that crashed left behind (macOS).
    let limits = proxy.clone();
    std::thread::spawn(move || {
        tailcat::sweep_leftovers();
        mount::sweep_leftovers();
        let _ = tailcat::prepare();
        let _ = limits.send_event(UserEvent::WebdavLimit(mount::size_limit()));
    });
    let ticker = proxy.clone();
    std::thread::spawn(move || {
        while ticker.send_event(UserEvent::Tick).is_ok() {
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    // Shows the window even if the page never says it's ready.
    let revealer = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let _ = revealer.send_event(UserEvent::Ipc(r#"{"cmd":"reveal"}"#.into()));
    });

    let mut app = App::new(proxy);
    // The name from last time, so nobody is asked again.
    let name = saved_name();
    if !name.is_empty() {
        app.set_name(&name);
    }

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let _ = &web_context;
        let changed = match event {
            // NSApp only exists once the loop is running.
            #[cfg(target_os = "macos")]
            Event::NewEvents(tao::event::StartCause::Init) => {
                menu.init_for_nsapp();
                false
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == MENU_QUIT {
                    app.shutdown();
                    *flow = ControlFlow::Exit;
                }
                false
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                app.shutdown();
                *flow = ControlFlow::Exit;
                false
            }
            Event::WindowEvent { event: WindowEvent::Resized(_), .. } => {
                send(&webview, json!({ "type": "window", "maximized": window.is_maximized() }));
                false
            }
            Event::UserEvent(UserEvent::Ipc(body)) => match serde_json::from_str::<Value>(&body) {
                Ok(msg) => {
                    if app.on_message(&msg, &window, &webview) {
                        app.shutdown();
                        *flow = ControlFlow::Exit;
                    }
                    // Screen video chunks arrive many times a second and change nothing on screen.
                    !matches!(msg["cmd"].as_str(), Some("voip.video" | "voip.keyframe" | "page"))
                }
                Err(_) => false,
            },
            Event::UserEvent(UserEvent::Tc(role, id, ev)) => app.on_tailcat(role, id, ev),
            Event::UserEvent(UserEvent::Share(ev)) => app.on_share(ev),
            Event::UserEvent(UserEvent::Recv(ev)) => app.on_recv(ev),
            Event::UserEvent(UserEvent::Dropped(paths)) if app.page == "voip" && app.voip.is_live() => {
                // Dropped on a room: send them into chat.
                app.voip.attach(paths.into_iter().filter(|p| p.is_file()).collect(), String::new(), &app.proxy);
                true
            }
            Event::UserEvent(UserEvent::Dropped(paths)) => {
                let n = share::add_paths(&app.share.lib, &paths);
                send(&webview, json!({ "type": "dropped", "count": n }));
                app.set_status(format!("Added {n} file{} to share", plural(n)), "ok");
                true
            }
            Event::UserEvent(UserEvent::Tick) => app.on_tick(),
            Event::UserEvent(UserEvent::Ping(target, result)) => app.on_ping(target, result),
            Event::UserEvent(UserEvent::Heard { share, heard }) => app.on_heard(share, heard),
            Event::UserEvent(UserEvent::Lan(key, result)) => app.on_lan(key, result),
            Event::UserEvent(UserEvent::Probed(found)) => app.on_probed(found),
            Event::UserEvent(UserEvent::MountInfo(key, r)) => app.on_mount_info(key, r),
            Event::UserEvent(UserEvent::MountDone(key, r)) => app.on_mount_done(key, r),
            Event::UserEvent(UserEvent::MountList(key, path, r)) => app.on_mount_list(key, path, r),
            Event::UserEvent(UserEvent::MountXfer(key, id, x)) => app.on_mount_xfer(key, id, x),
            Event::UserEvent(UserEvent::Hub(gen, ev)) => {
                if let Some((text, kind)) = app.hub.on_event(gen, ev) {
                    app.set_status(text, kind);
                }
                true
            }
            Event::UserEvent(UserEvent::HubKey(_, saved, result, open_after)) => {
                app.on_hub_key(saved, result, open_after);
                true
            }
            Event::UserEvent(UserEvent::HubScan(_, id, no, entry, result)) => {
                app.on_hub_scan(&id, no, entry, result);
                true
            }
            Event::UserEvent(UserEvent::WebdavLimit(limit)) => {
                app.webdav_limit = limit;
                true
            }
            Event::UserEvent(UserEvent::Firewall(result)) => {
                match result {
                    Ok(_) => app.set_status(
                        "Done. Windows asks again the next time a connection starts: reconnect, and press Allow.",
                        "ok",
                    ),
                    Err(e) => app.set_status(e, "bad"),
                }
                send(&webview, json!({ "type": "firewall", "ok": app.status.1 == "ok" }));
                true
            }
            Event::UserEvent(UserEvent::Voip(gen, ev)) => match app.voip.on_event(gen, ev) {
                // Talking lights and the mic meter go on their own, many times a second.
                Some(live) => {
                    if !live.is_null() {
                        send(&webview, live);
                    }
                    false
                }
                None => true,
            },
            _ => false,
        };
        if changed {
            send(&webview, app.snapshot());
        }
    });
}

/// The menu bar's Quit, the one item KittyConnect handles itself.
const MENU_QUIT: &str = "quit";

/// The macOS menu bar. The app menu is shown under the app's name; Edit is
/// there because WebKit only gets ⌘C/⌘V/⌘X/⌘A/⌘Z through menu items.
#[cfg(target_os = "macos")]
fn menu_bar(proxy: EventLoopProxy<UserEvent>) -> muda::Menu {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    let quit = Accelerator::new(Modifiers::META, Code::KeyQ);
    let app_menu = Submenu::with_items(
        "Portside",
        true,
        &[
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(MENU_QUIT, "Quit Portside", true, Some(quit)),
        ],
    )
    .expect("app menu");
    let edit_menu = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )
    .expect("edit menu");
    let window_menu = Submenu::with_items(
        "Window",
        true,
        &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None), &PredefinedMenuItem::close_window(None)],
    )
    .expect("window menu");
    let menu = Menu::with_items(&[&app_menu, &edit_menu, &window_menu]).expect("menu bar");
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(e.id.0));
    }));
    menu
}

/// "windows" or "macos", for the page.
const OS: &str = if cfg!(target_os = "macos") { "macos" } else { "windows" };

/// The name picked last time, kept in the data folder (`settings.json`) so
/// it isn't asked for every time.
fn saved_name() -> String {
    std::fs::read(data_dir().join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["name"].as_str().map(peers::clean_name))
        .unwrap_or_default()
}

fn save_name(name: &str) {
    let path = data_dir().join("settings.json");
    let mut v = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or(json!({}));
    v["name"] = json!(name);
    let tmp = data_dir().join("settings.json.part");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn page_html() -> String {
    let boot = json!({
        "version": VERSION,
        "tailcat": tailcat::VERSION,
        "dataDir": data_dir().display().to_string(),
        "os": OS,
        // The name from last time: used without asking again.
        "name": saved_name(),
    });
    INDEX_HTML
        .replace("/*{{CSS}}*/", APP_CSS)
        .replace("/*{{BOOT}}*/", &format!("window.BOOT = {boot};"))
        .replace("/*{{JS}}*/", APP_JS)
}

/// Answers the page's own requests: the app itself, and inline image previews
/// of room attachments (already fetched through the tunnel into the cache).
fn serve_page(html: &str, path: &str) -> wry::http::Response<std::borrow::Cow<'static, [u8]>> {
    use wry::http::Response;
    let reply = |status: u16, mime: &str, body: Vec<u8>| {
        Response::builder()
            .status(status)
            .header("Content-Type", mime)
            .header("Cache-Control", "no-store")
            .body(std::borrow::Cow::Owned(body))
            .unwrap()
    };
    if path == "/" || path == "/index.html" {
        return reply(200, "text/html; charset=utf-8", html.as_bytes().to_vec());
    }
    // /att/<room generation>/<attachment id>: numbers only, so nothing outside the cache is reachable.
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if let ["att", gen, aid] = parts[..] {
        if let (Ok(gen), Ok(aid)) = (gen.parse::<u64>(), aid.parse::<u32>()) {
            if let Ok(bytes) = std::fs::read(voip::preview_dir(gen).join(format!("a{aid}"))) {
                let mime = match bytes.get(..12) {
                    Some([0x89, b'P', b'N', b'G', ..]) => "image/png",
                    Some([0xFF, 0xD8, ..]) => "image/jpeg",
                    Some([b'G', b'I', b'F', ..]) => "image/gif",
                    Some([b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P']) => "image/webp",
                    Some([b'B', b'M', ..]) => "image/bmp",
                    _ => "application/octet-stream",
                };
                return reply(200, mime, bytes);
            }
        }
    }
    reply(404, "text/plain", b"not found".to_vec())
}

fn send(webview: &WebView, msg: Value) {
    let _ = webview.evaluate_script(&format!("window.onHost && window.onHost({msg})"));
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub(crate) fn free_port() -> Result<u16, String> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("Couldn't find a free local port: {e}"))
}

fn port_free(port: u16, lan: bool) -> bool {
    let ip = if lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    TcpListener::bind((ip, port)).is_ok()
}

fn something_listening(port: u16) -> bool {
    let v4 = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&v4, Duration::from_millis(250)).is_ok()
        || TcpStream::connect_timeout(&v6, Duration::from_millis(250)).is_ok()
}

/// The last useful thing TailCat said, for an error message.
pub(crate) fn last_words(log: &Log) -> String {
    log.0
        .iter()
        .rev()
        .find(|l| !l.starts_with("Portside:"))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .unwrap_or_default()
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> App {
        App {
            proxy,
            share: ShareHost {
                st: St::Off,
                error: String::new(),
                lib: Arc::new(Mutex::new(Vec::new())),
                server: None,
                proc: None,
                addr: None,
                sending: BTreeMap::new(),
                sent: 0,
                link: Link::default(),
                peers: Peers::default(),
                log: Log::default(),
            },
            recv: Recv {
                st: St::Off,
                error: String::new(),
                proc: None,
                port: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                jobs: None,
                files: Vec::new(),
                states: BTreeMap::new(),
                inbox: portable::default_inbox(),
                link: Link::default(),
                args: Vec::new(),
                peer: 0,
                addr: String::new(),
                host_name: None,
                log: Log::default(),
            },
            hosts: Vec::new(),
            joins: Vec::new(),
            host_error: String::new(),
            join_error: String::new(),
            next_key: 1,
            voip: Voip::default(),
            hub: hub::Hub::with_store(hubstore::Store::default()),
            page: "hub".into(),
            status: ("Ready. Everything runs from this folder.".into(), "idle"),
            name: String::new(),
            adverts: Vec::new(),
            listening: HashMap::new(),
            probing: false,
            probe_at: Instant::now(),
            mount_pick: None,
            mount_host_error: String::new(),
            mount_join_error: String::new(),
            webdav_limit: None,
            next_xfer: 1,
        }
    }

    fn set_status(&mut self, text: impl Into<String>, kind: &'static str) {
        self.status = (text.into(), kind);
    }

    fn shutdown(&mut self) {
        // Say goodbye to everyone we joined, all at once, so closing takes at
        // most about a second.
        let mut byes = vec![self.recv_bye()];
        byes.extend(self.joins.iter_mut().map(join_bye));
        for t in byes.into_iter().flatten() {
            let _ = t.join();
        }
        self.stop_share();
        self.stop_recv();
        self.hosts.clear();
        self.joins.clear();
        self.voip.shutdown();
        // Hosting a saved hub: someone in it takes over; in someone else's
        // hub: they see us go. Either way, a moment for them to hear.
        let joined = self.hub.is_on() && !self.hub.is_host();
        if self.hub.leave() || joined {
            std::thread::sleep(Duration::from_millis(800));
        }
    }

    /// Tells the sharer we're leaving, then closes the tunnel.
    fn recv_bye(&mut self) -> Option<std::thread::JoinHandle<()>> {
        if self.recv.st != St::Live || !self.recv.link.ok_once {
            return None;
        }
        let proc = self.recv.proc.take()?;
        let report = Report { peer: self.recv.peer, addr: &self.recv.addr, path: "unknown", rtt: None, ports: Vec::new() }.bye();
        let port = self.recv.port;
        Some(std::thread::spawn(move || {
            tailcat::say_bye(port, &report);
            drop(proc);
        }))
    }

    fn set_name(&mut self, name: &str) {
        let name = peers::clean_name(name);
        if name.is_empty() || name == self.name {
            return;
        }
        self.name = name;
        peers::set_my_name(&self.name);
        save_name(&self.name);
        self.hub.rename(&self.name);
        // Everyone we're connected to hears the new name now.
        let now = Instant::now();
        if self.recv.link.ok_once && !self.recv.link.pinging {
            self.recv.link.next_ping = Some(now);
        }
        for j in &mut self.joins {
            if !j.link.pinging {
                j.link.next_ping = Some(now);
            }
        }
    }

    /// Returns true when the app should exit.
    fn on_message(&mut self, msg: &Value, window: &Window, webview: &WebView) -> bool {
        let cmd = msg["cmd"].as_str().unwrap_or_default();
        let s = |k: &str| msg[k].as_str().unwrap_or_default().to_string();
        match cmd {
            "ready" | "reveal" => {
                // For build checks: proves the page loaded and its script ran.
                if cmd == "ready" {
                    if let Some(marker) = std::env::var_os("KC_READY_MARKER") {
                        let _ = std::fs::write(marker, format!("ready {}", s("origin")));
                    }
                }
                if !window.is_visible() {
                    window.set_visible(true);
                    window.set_focus();
                }
            }
            "window" => match s("action").as_str() {
                "drag" => {
                    let _ = window.drag_window();
                }
                "minimize" => window.set_minimized(true),
                "maximize" => window.set_maximized(!window.is_maximized()),
                "close" => return true,
                _ => {}
            },
            "resize" => {
                let dir = match s("dir").as_str() {
                    "n" => ResizeDirection::North,
                    "s" => ResizeDirection::South,
                    "e" => ResizeDirection::East,
                    "w" => ResizeDirection::West,
                    "ne" => ResizeDirection::NorthEast,
                    "nw" => ResizeDirection::NorthWest,
                    "se" => ResizeDirection::SouthEast,
                    "sw" => ResizeDirection::SouthWest,
                    _ => return false,
                };
                if !window.is_maximized() {
                    let _ = window.drag_resize_window(dir);
                }
            }

            // ------------------------------------------------ file share
            "share.addFiles" => {
                if let Some(files) = rfd::FileDialog::new().set_title("Choose files to share").set_parent(window).pick_files() {
                    let n = share::add_paths(&self.share.lib, &files);
                    self.set_status(format!("Added {n} file{}", plural(n)), "ok");
                }
            }
            "share.addFolder" => {
                if let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to share").set_parent(window).pick_folder() {
                    let n = share::add_paths(&self.share.lib, &[dir]);
                    self.set_status(format!("Added {n} file{} from the folder", plural(n)), "ok");
                }
            }
            "share.remove" => {
                let id = msg["id"].as_u64().unwrap_or(0);
                self.share.lib.lock().unwrap().retain(|f| f.id != id);
            }
            "share.clear" => self.share.lib.lock().unwrap().clear(),
            "share.start" => self.start_share(),
            "share.stop" => {
                self.stop_share();
                self.set_status("Stopped sharing. The old code no longer works.", "idle");
            }

            // ---------------------------------------------- file receive
            "recv.connect" => self.start_recv(&s("code")),
            "recv.stop" => {
                self.recv_bye();
                self.stop_recv();
                self.set_status("Disconnected", "idle");
            }
            "recv.refresh" => {
                if self.recv.st == St::Live {
                    self.recv.st = St::Starting;
                    share::fetch_list(self.recv.port, self.recv.cancel.clone(), self.proxy.clone());
                }
            }
            "recv.get" => {
                let id = msg["id"].as_u64().unwrap_or(0);
                self.queue(&[id]);
            }
            "recv.getAll" => {
                let ids: Vec<u64> = self.recv.files.iter().map(|f| f.id).collect();
                self.queue(&ids);
            }
            "recv.pickInbox" => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Save received files to")
                    .set_directory(&self.recv.inbox)
                    .set_parent(window)
                    .pick_folder()
                {
                    self.recv.inbox = dir;
                }
            }
            "recv.openInbox" => {
                let _ = std::fs::create_dir_all(&self.recv.inbox);
                open_in_shell(&self.recv.inbox);
            }
            "recv.reveal" => {
                let id = msg["id"].as_u64().unwrap_or(0);
                if let Some(path) = self.recv.states.get(&id).and_then(|s| s.path.clone()) {
                    reveal_in_shell(&path);
                }
            }

            // ------------------------------------------------------ ports
            "port.host" => self.start_port_host(&s("ports"), None),
            "game.host" => {
                let game = s("game");
                if code::is_game_id(&game) {
                    self.start_port_host(&s("port"), Some(game));
                }
            }
            "game.join" => self.start_port_join(&s("code"), "", &[], false, JoinFrom::Game),
            "port.hostStop" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(h) = self.hosts.iter().find(|h| h.key == key) {
                    if let Some(m) = &h.mount {
                        let name = m.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        self.set_status(format!("Stopped sharing {name}. Its code no longer works."), "idle");
                    } else {
                        let list = h.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
                        self.set_status(format!("Closed port {list}. Its code no longer works."), "idle");
                    }
                }
                self.hosts.retain(|h| h.key != key);
            }
            "port.join" => {
                let locals: Vec<u16> = msg["locals"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(0) as u16).collect())
                    .unwrap_or_default();
                self.start_port_join(&s("code"), &s("ports"), &locals, msg["lan"].as_bool().unwrap_or(false), JoinFrom::Manual);
            }
            "port.joinStop" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(j) = self.joins.iter_mut().find(|j| j.key == key) {
                    join_bye(j);
                }
                self.joins.retain(|j| j.key != key);
                self.set_status("Disconnected", "idle");
            }

            // ------------------------------------------------------ mounts
            "mount.pick" => {
                if let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to share").set_parent(window).pick_folder() {
                    self.mount_host_error.clear();
                    self.mount_pick = Some(dir);
                }
            }
            "mount.host" => self.start_mount_host(msg["write"].as_bool().unwrap_or(false)),
            "mount.join" => {
                self.start_port_join(&s("code"), "", &[], false, JoinFrom::Mount);
                self.mount_join_error = std::mem::take(&mut self.join_error);
            }
            "mount.map" => self.mount_map(msg["key"].as_u64().unwrap_or(0)),
            "mount.unmap" => self.mount_unmap(msg["key"].as_u64().unwrap_or(0)),
            "mount.open" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(m) = self.joins.iter().find(|j| j.key == key).and_then(|j| j.mount.as_ref()?.mounted.as_ref()) {
                    mount::open(m);
                }
            }
            // No path: stay where it is (Refresh).
            "mount.browse" => self.mount_browse(msg["key"].as_u64().unwrap_or(0), msg["path"].as_str().map(str::to_string)),
            "mount.browseClose" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(m) = self.joins.iter_mut().find(|j| j.key == key).and_then(|j| j.mount.as_mut()) {
                    m.browse = None;
                }
            }
            "mount.get" => self.mount_get(msg["key"].as_u64().unwrap_or(0), &s("path"), msg["dir"].as_bool().unwrap_or(false)),
            "mount.put" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(files) = rfd::FileDialog::new().set_title("Upload to the shared folder").set_parent(window).pick_files() {
                    self.mount_put(key, &s("path"), files);
                }
            }
            "mount.cancel" => {
                let (key, id) = (msg["key"].as_u64().unwrap_or(0), msg["id"].as_u64().unwrap_or(0));
                if let Some(t) = self.joins.iter().find(|j| j.key == key).and_then(|j| j.mount.as_ref()?.transfers.iter().find(|t| t.id == id)) {
                    t.cancel.store(true, Ordering::Relaxed);
                }
            }
            "mount.reveal" => {
                let (key, id) = (msg["key"].as_u64().unwrap_or(0), msg["id"].as_u64().unwrap_or(0));
                if let Some(p) = self.joins.iter().find(|j| j.key == key).and_then(|j| j.mount.as_ref()?.transfers.iter().find(|t| t.id == id)?.path.clone()) {
                    reveal_in_shell(&p);
                }
            }
            "mount.bigFiles" => {
                let proxy = self.proxy.clone();
                self.set_status("Waiting for Windows to allow bigger files…", "busy");
                std::thread::spawn(move || {
                    #[cfg(windows)]
                    {
                        let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
                        let _ = firewall::run_elevated(&me, "--kc-webdav-big-files", "change how big a file Windows' WebDAV opens");
                    }
                    let _ = proxy.send_event(UserEvent::WebdavLimit(mount::size_limit()));
                });
            }

            // ------------------------------------------------------- voip
            "name.set" => self.set_name(&s("name")),

            // ------------------------------------------- relay → direct
            "recv.retry" => self.retry_recv(),
            "port.retry" => self.retry_join(msg["key"].as_u64().unwrap_or(0)),
            "peer.retry" => {
                // Ids go to the page as strings: they don't fit in a JS number.
                if let Ok(id) = s("id").parse::<u64>() {
                    let _ = self.share.peers.retry(id) || self.hosts.iter_mut().any(|h| h.peers.retry(id));
                }
            }
            // Windows Firewall only; the page doesn't offer it elsewhere.
            "firewall.reset" if cfg!(windows) => {
                let proxy = self.proxy.clone();
                self.set_status("Waiting for Windows to allow the change…", "busy");
                std::thread::spawn(move || {
                    let _ = proxy.send_event(UserEvent::Firewall(firewall::reset()));
                });
            }
            "voip.host" => {
                self.voip.host(&s("room"), &self.name, &self.proxy);
                self.set_status("Opening your room…", "busy");
            }
            "voip.join" => {
                self.voip.join(&s("code"), &self.name, &self.proxy);
                self.set_status("Joining the room through TailCat…", "busy");
            }
            "voip.leave" => {
                self.voip.leave();
                self.set_status("Left the room", "idle");
            }
            "voip.chat" => self.voip.chat(&s("text")),
            "voip.voice" => self.voip.voice(msg["on"].as_bool().unwrap_or(false), &self.proxy),
            "voip.mute" => self.voip.set_mute(msg["muted"].as_bool().unwrap_or(false), msg["deaf"].as_bool().unwrap_or(false)),
            "voip.screen" => self.voip.screen(msg.get("info").filter(|v| v.is_object()).cloned()),
            "voip.video" => {
                use base64::Engine as _;
                if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(s("data")) {
                    // Fell behind: the page's encoder makes a key frame to recover from.
                    let layer = msg["layer"].as_u64().unwrap_or(0).min(3) as u8;
                    if self.voip.video(msg["key"].as_bool().unwrap_or(false), layer, msg["ts"].as_u64().unwrap_or(0), &data) {
                        send(webview, json!({ "type": "keyframe" }));
                    }
                }
            }
            "voip.watch" => self.voip.watch(msg["id"].as_u64().map(|v| v as u16)),
            // Native screen share: list what can be shared, start, stop.
            "voip.sources" => self.voip.list_sources(&self.proxy),
            "voip.share" => self.voip.share(msg, &self.proxy),
            "voip.unshare" => self.voip.unshare(),
            "voip.preview" => self.voip.set_preview(msg["on"].as_bool().unwrap_or(false)),
            "voip.shareVolume" => self.voip.set_share_volume(msg["volume"].as_f64().unwrap_or(1.0) as f32),
            "voip.keyframe" => self.voip.request_keyframe(msg["id"].as_u64().unwrap_or(0) as u16),
            "voip.attach" => {
                if let Some(files) = rfd::FileDialog::new().set_title("Send files in chat").set_parent(window).pick_files() {
                    self.voip.attach(files, s("text"), &self.proxy);
                }
            }
            "voip.download" => self.voip.download(msg["aid"].as_u64().unwrap_or(0) as u32, self.recv.inbox.clone()),
            "voip.reveal" => {
                if let Some(path) = self.voip.download_path(msg["aid"].as_u64().unwrap_or(0) as u32) {
                    reveal_in_shell(&path);
                }
            }
            "voip.volume" => self.voip.set_volume(msg["id"].as_u64().unwrap_or(0) as u16, msg["gain"].as_f64().unwrap_or(1.0) as f32),
            "voip.mod" => self.voip.moderate(&s("action"), msg["id"].as_u64().unwrap_or(0) as u16),
            "voip.audio" => self.voip.set_audio(&msg["settings"]),
            "window.fullscreen" => {
                let on = msg["on"].as_bool().unwrap_or(false);
                window.set_fullscreen(on.then_some(tao::window::Fullscreen::Borderless(None)));
            }
            "page" => self.page = s("page"),

            // --------------------------------------------------------- hub
            // A new hub: temporary (gone when it closes), or saved for `secs`.
            "hub.host" => {
                if msg["kind"] == "saved" {
                    let secs = msg["secs"].as_u64().unwrap_or(0);
                    self.hub.create_saved(&s("name"), secs, &self.name, &self.proxy);
                    if self.hub.error.is_empty() {
                        self.set_status("Making your hub's key (a few seconds)…", "busy");
                    }
                } else {
                    self.hub.host_temporary(&s("name"), &self.name, &self.proxy);
                    if self.hub.error.is_empty() {
                        self.set_status("Opening your hub…", "busy");
                    }
                }
            }
            // A saved hub: Open looks for whoever has it open first.
            "hub.open" => {
                let id = s("id");
                match self.hub.scan(&id, true, &self.proxy) {
                    Some(hub::AfterScan::OpenOwn(id)) => self.open_saved_hub(&id),
                    Some(hub::AfterScan::Said(text, kind)) => self.set_status(text, kind),
                    None => self.set_status("Checking whether it's already online… This can take some time.", "busy"),
                }
            }
            "hub.scan" => {
                let id = s("id");
                if self.hub.scan(&id, false, &self.proxy).is_none() {
                    self.set_status("Scanning for whoever has it open… This can take some time.", "busy");
                }
            }
            "hub.joinCode" => {
                self.hub.join(&s("code"), &self.name, &self.proxy);
                if self.hub.error.is_empty() {
                    self.set_status("Joining the hub through TailCat…", "busy");
                }
            }
            "hub.dismissPrompt" => self.hub.dismiss_prompt(),
            "hub.save" => {
                self.hub.save_joined(&self.proxy);
                if self.hub.error.is_empty() {
                    self.set_status("Saving the hub (a few seconds)…", "busy");
                }
            }
            "hub.close" => {
                if self.hub.leave() {
                    self.set_status("Closed your copy. Someone else in it took over, so it's still open for everyone; Scan finds it.", "ok");
                } else {
                    self.set_status("Closed the hub. It's still saved: open it again any time before it expires.", "idle");
                }
            }
            "hub.delete" => {
                if let Some((text, kind)) = self.hub.delete(&s("id"), msg["everyone"] == true) {
                    self.set_status(text, kind);
                }
            }
            "hub.openToHub" => self.hub.set_open(msg["on"] == true),
            "hub.acceptOffer" => {
                if let Some(a) = self.hub.accept_offer(msg["from"].as_u64().unwrap_or(0), &s("oid")) {
                    let (page, view) = self.join_invite(&a);
                    send(webview, json!({ "type": "goto", "page": page, "view": view }));
                }
            }
            "hub.join" => {
                self.hub.join(&s("code"), &self.name, &self.proxy);
                if self.hub.error.is_empty() {
                    self.set_status("Joining the hub through TailCat…", "busy");
                }
            }
            "hub.leave" => {
                let closing = self.hub.is_host();
                self.hub.leave();
                self.set_status(if closing { "Deleted your hub. Its code no longer works." } else { "Left the hub" }, "idle");
            }
            "hub.invite" => {
                let to: Vec<u64> = msg["to"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
                let what = s("what");
                let found = self.invitables().into_iter().find(|i| i.what == what);
                let result = match found {
                    Some(i) => self.hub.invite(&i.code, &i.title, to),
                    None => Err("That isn't open any more.".into()),
                };
                if let Err(e) = result {
                    self.set_status(e, "bad");
                }
            }
            "hub.accept" => {
                if let Some(a) = self.hub.accept(msg["id"].as_u64().unwrap_or(0)) {
                    let (page, view) = self.join_invite(&a);
                    send(webview, json!({ "type": "goto", "page": page, "view": view }));
                }
            }
            "hub.dismiss" => self.hub.dismiss(msg["id"].as_u64().unwrap_or(0)),
            "hub.kick" => self.hub.kick(msg["id"].as_u64().unwrap_or(0)),
            "conn.drop" => self.drop_connection(&s("id")),

            // ------------------------------------------------------- misc
            "copy" => {
                let ok = arboard::Clipboard::new().and_then(|mut c| c.set_text(s("text"))).is_ok();
                if ok {
                    self.set_status(if msg["code"].as_bool() == Some(true) {
                        "Code copied. Send it to your friend (it's safe to paste into Discord)."
                    } else {
                        "Copied to the clipboard"
                    }, "ok");
                } else {
                    self.set_status(if cfg!(target_os = "macos") {
                        "Couldn't reach the clipboard. Select the code and press \u{2318}C."
                    } else {
                        "Couldn't reach the clipboard. Select the code and press Ctrl+C."
                    }, "bad");
                }
            }
            "paste" => {
                let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                send(webview, json!({ "type": "paste", "target": s("target"), "text": text }));
                return false;
            }
            "openData" => open_in_shell(data_dir()),
            _ => return false,
        }
        false
    }

    /// Someone connected to what we opened checked in or left.
    fn on_heard(&mut self, share: bool, heard: Heard) -> bool {
        if !heard.is_named() {
            return false;
        }
        if share {
            return self.share.st == St::Live && self.share.peers.heard(&heard);
        }
        // All open ports share one beacon; the code they joined with says which.
        match self.hosts.iter_mut().find(|h| h.proc.is_some() && h.addr.as_deref() == Some(heard.addr.as_str())) {
            Some(h) => h.peers.heard(&heard),
            None => false,
        }
    }

    fn snapshot(&self) -> Value {
        let files = self.share.lib.lock().unwrap().clone();
        let total: u64 = files.iter().map(|f| f.size).sum();
        let share_code = match (&self.share.addr, &self.share.server) {
            (Some(a), Some(s)) => Some(code::file(s.port, a)),
            _ => None,
        };
        let recv_files: Vec<Value> = self
            .recv
            .files
            .iter()
            .map(|f| {
                let s = self.recv.states.get(&f.id);
                json!({
                    "id": f.id, "name": f.name, "size": f.size,
                    "st": s.map(|s| s.st).unwrap_or("idle"),
                    "done": s.map(|s| s.done).unwrap_or(0),
                    "total": s.map(|s| s.total).unwrap_or(f.size),
                    "speed": s.map(|s| s.speed.bps as u64).unwrap_or(0),
                    "error": s.map(|s| s.error.clone()).unwrap_or_default(),
                })
            })
            .collect();
        json!({
            "type": "state",
            "status": { "text": self.status.0, "kind": self.status.1 },
            "name": self.name,
            "share": {
                "st": self.share.st.name(),
                "error": self.share.error,
                "code": share_code,
                "files": files,
                "totalSize": total,
                "sending": self.share.sending.values().map(|s| json!({
                    "name": s.name, "done": s.done, "total": s.total, "speed": s.speed.bps as u64,
                })).collect::<Vec<_>>(),
                "sent": self.share.sent,
                "peers": self.share.peers.json(),
                "log": self.share.log.json(),
            },
            "recv": {
                "st": self.recv.st.name(),
                "error": self.recv.error,
                "files": recv_files,
                "inbox": self.recv.inbox.display().to_string(),
                "link": self.recv.link.json(),
                "hostName": self.recv.host_name,
                "log": self.recv.log.json(),
            },
            "voip": self.voip.json(),
            "hub": self.hub.json(),
            "invitables": self.invitables().iter().map(|i| json!({ "what": i.what, "kind": i.kind, "title": i.title })).collect::<Vec<_>>(),
            "connections": self.connections(),
            "hostError": self.host_error,
            "joinError": self.join_error,
            "mounts": {
                "pick": self.mount_pick.as_ref().map(|p| p.display().to_string()),
                "hostError": self.mount_host_error,
                "joinError": self.mount_join_error,
                // Windows' WebDAV limit on file size (None on a Mac).
                "limit": self.webdav_limit,
                "os": OS,
            },
            "hosts": self.hosts.iter().map(|h| json!({
                "key": h.key,
                "st": h.st.name(),
                "error": h.error,
                "warn": h.warn,
                "ports": h.ports,
                "code": match (&h.game, &h.mount, &h.addr) {
                    (Some(g), _, Some(a)) => Some(code::game(g, h.ports[0], a)),
                    (_, Some(_), Some(a)) => Some(code::mount(h.ports[0], a)),
                    _ => h.addr.clone(),
                },
                "game": h.game,
                "mount": h.mount.as_ref().map(|m| json!({
                    "folder": m.dir.display().to_string(),
                    "name": m.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "write": m.write,
                })),
                "lanNote": h.lan_note,
                "peers": h.peers.json(),
                "log": h.log.json(),
            })).collect::<Vec<_>>(),
            "joins": self.joins.iter().map(|j| json!({
                "key": j.key,
                // "unreachable": joining, and the host hasn't answered for a while.
                "st": if j.unreachable() { "unreachable" } else { j.st.name() },
                "error": j.error,
                "lan": j.lan,
                "maps": j.maps.iter().map(|(l, r)| json!({ "local": l, "remote": r })).collect::<Vec<_>>(),
                "link": j.link.json(),
                "hostName": j.host_name,
                "game": j.game,
                "worlds": j.worlds,
                "mount": j.mount.as_ref().map(|m| json!({
                    "name": m.info.as_ref().map(|i| if i.title.is_empty() { i.name.clone() } else { i.title.clone() }),
                    "write": m.info.as_ref().is_some_and(|i| i.write),
                    "mounted": m.mounted.as_ref().map(mount::Mounted::label),
                    "busy": m.busy,
                    "error": m.error,
                    "browse": m.browse.as_ref().map(|b| json!({
                        "path": b.path, "entries": b.entries, "loading": b.loading, "error": b.error,
                    })),
                    "transfers": m.transfers.iter().map(|t| json!({
                        "id": t.id, "name": t.name, "up": t.up, "done": t.done, "total": t.total,
                        "st": t.st, "error": t.error, "speed": t.speed.bps as u64, "saved": t.path.is_some(),
                    })).collect::<Vec<_>>(),
                })),
                "checks": j.beacon.is_some(),
                "log": j.log.json(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Tells a port host we're leaving, then closes the tunnel. A mounted share
/// is unmounted first: a drive whose server is gone makes Explorer and Finder
/// wait on it.
fn join_bye(j: &mut PortJoin) -> Option<std::thread::JoinHandle<()>> {
    let mounted = j.mount.as_mut().and_then(|m| m.mounted.take());
    let bye = j.beacon.filter(|_| j.link.ok_once).map(|port| (port, j.report().bye()));
    if mounted.is_none() && bye.is_none() {
        return None;
    }
    let proc = j.proc.take();
    Some(std::thread::spawn(move || {
        if let Some(m) = mounted {
            mount::unmount(&m);
        }
        if let Some((port, report)) = bye {
            tailcat::say_bye(port, &report);
        }
        drop(proc);
    }))
}

/// Opens a folder (or a web page) the way double-clicking it would.
pub(crate) fn open_in_shell(target: impl AsRef<std::ffi::OsStr>) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(target).spawn();
}

/// Shows a file selected in its folder: Explorer on Windows, Finder on a Mac.
fn reveal_in_shell(path: &std::path::Path) {
    if cfg!(windows) {
        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    } else if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    } else if let Some(dir) = path.parent() {
        open_in_shell(dir);
    }
}

#[cfg(windows)]
fn missing_webview2(detail: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDOK, MB_ICONWARNING, MB_OKCANCEL};
    let text = format!(
        "Portside needs the Microsoft Edge WebView2 Runtime, which comes with Windows 11 \
         and most Windows 10 installs, but couldn't start it here.\n\n\
         Press OK to open Microsoft's download page.\n\n({detail})"
    );
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(&text), wide("Portside"));
    let choice = unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OKCANCEL | MB_ICONWARNING) };
    if choice == IDOK {
        open_in_shell("https://developer.microsoft.com/microsoft-edge/webview2/");
    }
}

#[cfg(not(windows))]
fn missing_webview2(detail: &str) {
    eprintln!("Portside couldn't start the system WebView ({detail})");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joining(since: Duration, beacon: Option<u16>) -> PortJoin {
        let now = Instant::now();
        PortJoin {
            key: 1,
            st: St::Starting,
            error: String::new(),
            proc: Some(Proc::stand_in()),
            maps: vec![(12345, 12345)],
            lan: false,
            started: now.checked_sub(since).unwrap_or(now),
            beacon,
            link: Link::default(),
            told_unreachable: false,
            ports_checked: false,
            args: Vec::new(),
            peer: 1,
            addr: String::new(),
            host_name: None,
            game: None,
            worlds: Vec::new(),
            lan_next: now,
            lan_asking: false,
            mount: None,
            relays: Vec::new(),
            log: Log::default(),
        }
    }

    const PONG: Result<Duration, String> = Ok(Duration::from_millis(20));

    /// TailCat listening on this PC proves nothing (a wrong code does that
    /// too), and nor does a tunnel to the host, even a direct one (a wrong
    /// port number does that): "Connected" waits for the host to answer and
    /// say the port is open on its code.
    #[test]
    fn a_joined_port_is_connected_only_once_the_host_says_the_port_is_open() {
        let mut j = joining(Duration::from_secs(10), Some(40000));
        assert!(!j.promote() && !j.unreachable());
        assert_eq!(j.st, St::Starting);
        assert_eq!(j.checked(Err("no answer".into()), None), Checked::Nothing);
        assert_eq!(j.checked(PONG, Some(PortCheck::Open)), Checked::Connected);
        assert_eq!(j.st, St::Live);
        // The check passed: later checks don't send the ports again.
        assert!(j.report().ports.is_empty());

        let mut j = joining(Duration::from_secs(1), Some(40000));
        assert_eq!(j.report().ports, vec![12345]);
        j.link.observe("2026/11/12 01:02:03 magicsock: disco: node [a] d:1 now using 10.0.0.5:41641 mtu=1360");
        assert!(!j.promote(), "a direct path proves the code, not the port");
        assert_eq!(j.link.path(), "direct");
        // An older host answers without a port check: as before, the answer is enough.
        assert_eq!(j.checked(PONG, None), Checked::Connected);
    }

    /// The 25566 case: the code is right, the port isn't. It stops with an
    /// error instead of showing "Connected · Direct", and the tunnel closes,
    /// so nothing on this PC is left listening on that port.
    #[test]
    fn a_port_the_host_did_not_open_stops_the_join() {
        let mut j = joining(Duration::from_secs(5), Some(40000));
        j.maps = vec![(25566, 25566)];
        // Shared on the network too.
        j.relays.push(loopback::lan_relay_anywhere(free_port().unwrap()).0);
        j.link.observe("2026/11/12 01:02:03 magicsock: disco: node [a] d:1 now using 10.0.0.5:41641 mtu=1360");
        assert_eq!(j.checked(PONG, Some(PortCheck::Wrong)), Checked::WrongPorts);
        assert_eq!(j.st, St::Error);
        assert!(j.proc.is_none(), "the tunnel (and its listener on 25566) is closed");
        assert!(j.relays.is_empty(), "and nothing is left shared on the network");
        assert!(j.error.contains("25566"), "{}", j.error);
        assert!(!j.promote());

        // Asked too often just now: still waiting, not connected.
        let mut j = joining(Duration::from_secs(5), Some(40000));
        assert_eq!(j.checked(PONG, Some(PortCheck::Busy)), Checked::Nothing);
        assert_eq!(j.st, St::Starting);
        assert_eq!(j.checked(PONG, Some(PortCheck::Open)), Checked::Connected);
    }

    /// Nothing answers for 30 seconds: it says so, and keeps trying.
    #[test]
    fn a_host_that_never_answers_is_said_to_be_unreachable() {
        let j = joining(Duration::from_secs(29), Some(40000));
        assert!(!j.unreachable());
        let mut j = joining(UNREACHED + Duration::from_secs(1), Some(40000));
        assert_eq!(j.checked(Err("no answer".into()), None), Checked::Nothing);
        assert!(j.unreachable());
        // They come online after all.
        assert_eq!(j.checked(PONG, Some(PortCheck::Open)), Checked::Connected);
        assert!(!j.unreachable());
    }

    /// Only joining waits: with no beacon to ask (the friend's own port is
    /// the beacon's), a few quiet seconds have to do.
    #[test]
    fn without_a_beacon_a_few_quiet_seconds_do() {
        assert!(!joining(Duration::from_secs(1), None).promote());
        assert!(joining(Duration::from_secs(4), None).promote());
    }
}
