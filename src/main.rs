// No console window in release builds; debug builds keep one for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod beacon;
mod code;
mod firewall;
mod icon;
mod lan;
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
use peers::{Heard, Peers, Report};
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// Its ports have been checked for a server at least once.
    probed: bool,
    /// For Minecraft: what friends see, once it's showing.
    lan_note: String,
    log: Log,
}

impl PortHost {
    /// Stops telling joiners about LAN worlds on this code.
    fn unserve(&mut self) {
        if let (Some(_), Some(addr)) = (&self.game, &self.addr) {
            lan::unserve(addr);
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
    log: Log,
}

impl PortJoin {
    fn report(&self) -> Report<'_> {
        Report { peer: self.peer, addr: &self.addr, path: self.link.path(), rtt: self.link.rtt_ms }
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
}

fn main() {
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
        .with_title("KittyConnect")
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
    std::thread::spawn(|| {
        tailcat::sweep_leftovers();
        let _ = tailcat::prepare();
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
        "KittyConnect",
        true,
        &[
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(MENU_QUIT, "Quit KittyConnect", true, Some(quit)),
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

fn page_html() -> String {
    let boot = json!({
        "version": VERSION,
        "tailcat": tailcat::VERSION,
        "dataDir": data_dir().display().to_string(),
        "os": OS,
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
        .find(|l| !l.starts_with("KittyConnect:"))
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
            page: "files".into(),
            status: ("Ready. Everything runs from this folder.".into(), "idle"),
            name: String::new(),
            adverts: Vec::new(),
            listening: HashMap::new(),
            probing: false,
            probe_at: Instant::now(),
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
    }

    /// Tells the sharer we're leaving, then closes the tunnel.
    fn recv_bye(&mut self) -> Option<std::thread::JoinHandle<()>> {
        if self.recv.st != St::Live || !self.recv.link.ok_once {
            return None;
        }
        let proc = self.recv.proc.take()?;
        let report = Report { peer: self.recv.peer, addr: &self.recv.addr, path: "unknown", rtt: None }.bye();
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
            "game.join" => self.start_port_join(&s("code"), "", &[], false, true),
            "port.hostStop" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(h) = self.hosts.iter().find(|h| h.key == key) {
                    let list = h.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
                    self.set_status(format!("Closed port {list}. Its code no longer works."), "idle");
                }
                self.hosts.retain(|h| h.key != key);
            }
            "port.join" => {
                let locals: Vec<u16> = msg["locals"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(0) as u16).collect())
                    .unwrap_or_default();
                self.start_port_join(&s("code"), &s("ports"), &locals, msg["lan"].as_bool().unwrap_or(false), false);
            }
            "port.joinStop" => {
                let key = msg["key"].as_u64().unwrap_or(0);
                if let Some(j) = self.joins.iter_mut().find(|j| j.key == key) {
                    join_bye(j);
                }
                self.joins.retain(|j| j.key != key);
                self.set_status("Disconnected", "idle");
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

            // ------------------------------------------------------- misc
            "copy" => {
                let ok = arboard::Clipboard::new().and_then(|mut c| c.set_text(s("text"))).is_ok();
                if ok {
                    self.set_status(if msg["discord"].as_bool() == Some(true) {
                        "Copied for Discord. Paste it in a message."
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

    // ------------------------------------------------------------ sharing

    fn start_share(&mut self) {
        if self.share.st == St::Starting || self.share.st == St::Live {
            return;
        }
        if self.share.lib.lock().unwrap().is_empty() {
            self.share.st = St::Error;
            self.share.error = "Add some files or a folder first.".into();
            return;
        }
        self.share.error.clear();
        self.share.log = Log::default();
        self.share.link = Link::default();
        self.share.peers.clear();
        let server = match Server::start(self.share.lib.clone(), self.proxy.clone()) {
            Ok(s) => s,
            Err(e) => return self.fail_share(e),
        };
        let args = vec!["serve".into(), "--key=new".into(), server.port.to_string()];
        self.share.log.push(format!("KittyConnect: file server on 127.0.0.1:{}, starting TailCat", server.port));
        match tailcat::spawn(Role::ShareHost, &args, &self.proxy) {
            Ok(p) => {
                self.share.proc = Some(p);
                self.share.server = Some(server);
                self.share.st = St::Starting;
                self.set_status("Starting TailCat…", "busy");
            }
            Err(e) => self.fail_share(e),
        }
    }

    fn fail_share(&mut self, e: String) {
        self.share.st = St::Error;
        self.share.error = e.clone();
        self.set_status(e, "bad");
    }

    fn stop_share(&mut self) {
        self.share.st = St::Off;
        self.share.proc = None;
        self.share.server = None;
        self.share.addr = None;
        self.share.sending.clear();
        self.share.peers.clear();
    }

    // ---------------------------------------------------------- receiving

    fn start_recv(&mut self, input: &str) {
        self.stop_recv();
        self.recv.error.clear();
        self.recv.log = Log::default();
        self.recv.link = Link::default();
        self.recv.host_name = None;
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::File => c,
            Ok(c) if c.kind == Kind::Port => return self.fail_recv("That's a port code. Paste it in the Port Forward tab.".into()),
            Ok(c) if c.kind == Kind::Game => {
                return self.fail_recv("That's a game server code. Paste it in Port Forward, under Game servers.".into())
            }
            Ok(c) if c.kind == Kind::Voip => return self.fail_recv("That's a room code. Paste it in the VOIP tab.".into()),
            Ok(_) => return self.fail_recv("That's a port code. Paste it in the Port Forward tab, or ask for the file code (it starts with kitty:file:).".into()),
            Err(e) => return self.fail_recv(e),
        };
        let local = match free_port() {
            Ok(p) => p,
            Err(e) => return self.fail_recv(e),
        };
        let args: Vec<String> = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        self.recv.log.push(format!("KittyConnect: tunnelling 127.0.0.1:{local} to the sharer"));
        match tailcat::spawn(Role::RecvForward, &args, &self.proxy) {
            Ok(p) => {
                let cancel = Arc::new(AtomicBool::new(false));
                self.recv.proc = Some(p);
                self.recv.port = local;
                self.recv.args = args;
                self.recv.peer = peers::new_id();
                self.recv.addr = c.addr.clone();
                self.recv.cancel = cancel.clone();
                self.recv.jobs = Some(share::downloader(local, cancel.clone(), self.proxy.clone()));
                self.recv.st = St::Starting;
                share::fetch_list(local, cancel, self.proxy.clone());
                self.set_status("Connecting through TailCat… the first connection can take a few seconds", "busy");
            }
            Err(e) => self.fail_recv(e),
        }
    }

    fn fail_recv(&mut self, e: String) {
        self.stop_recv();
        self.recv.st = St::Error;
        self.recv.error = e.clone();
        self.set_status(e, "bad");
    }

    fn stop_recv(&mut self) {
        self.recv.cancel.store(true, Ordering::Relaxed);
        self.recv.jobs = None;
        self.recv.proc = None;
        self.recv.st = St::Off;
        self.recv.files.clear();
        self.recv.states.clear();
    }

    fn queue(&mut self, ids: &[u64]) {
        let Some(jobs) = &self.recv.jobs else { return };
        let mut n = 0;
        for id in ids {
            let busy = self.recv.states.get(id).is_some_and(|s| s.st == "queued" || s.st == "downloading");
            if busy {
                continue;
            }
            if let Some(file) = self.recv.files.iter().find(|f| f.id == *id).cloned() {
                let total = file.size;
                if jobs.send(share::Job { file, dest: self.recv.inbox.clone() }).is_ok() {
                    self.recv.states.insert(
                        *id,
                        FileState { st: "queued", done: 0, total, speed: Speed::new(0), path: None, error: String::new() },
                    );
                    n += 1;
                }
            }
        }
        if n > 0 {
            self.set_status(format!("Downloading {n} file{}…", plural(n)), "busy");
        }
    }

    // --------------------------------------------------------------- ports

    fn key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    /// Opens another port set; each one is listed with its own code and Stop.
    fn start_port_host(&mut self, input: &str, game: Option<String>) {
        self.host_error.clear();
        let ports = match code::parse_ports(input) {
            Ok(p) => p,
            Err(e) => return self.host_error = e,
        };
        if game.is_some() && ports.len() != 1 {
            return self.host_error = "A game server opens one port.".into();
        }
        // Minecraft: hear this PC's own "Open to LAN" so friends can see the world.
        if game.as_deref() == Some("minecraft") && !lan::listen() {
            self.host_error = "Couldn't listen for Minecraft LAN worlds (UDP 4445). The port still opens; friends can add the server by address.".into();
        }
        if let Some(p) = ports.iter().find(|p| self.hosts.iter().any(|h| h.proc.is_some() && h.ports.contains(p))) {
            return self.host_error = format!("Port {p} is already open below.");
        }
        // Whether a server is running there yet is checked in the background (`probe`).
        let warn = String::new();
        let mut list: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
        // The beacon rides in the same tunnel so the friend can tell we're still here.
        if let Some(b) = beacon::ensure(&self.proxy).filter(|b| !ports.contains(b)) {
            list.push(b.to_string());
        }
        let args = vec!["serve".into(), "--key=new".into(), list.join(",")];
        match tailcat::spawn(Role::PortHost, &args, &self.proxy) {
            Ok(p) => {
                let key = self.key();
                self.hosts.insert(0, PortHost {
                    key,
                    st: St::Starting,
                    error: String::new(),
                    warn,
                    proc: Some(p),
                    ports,
                    addr: None,
                    link: Link::default(),
                    peers: Peers::default(),
                    game,
                    probed: false,
                    lan_note: String::new(),
                    log: Log::default(),
                });
                self.set_status("Starting TailCat…", "busy");
            }
            Err(e) => self.host_error = e,
        }
    }

    /// `game_mode`: joined from Game servers, where the code carries the game
    /// and its port, and a busy local port is swapped for a free one.
    fn start_port_join(&mut self, input: &str, ports_input: &str, locals: &[u16], lan: bool, game_mode: bool) {
        self.join_error.clear();
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::File => {
                return self.join_error = "That's a file share code. Paste it in the File Share tab.".into()
            }
            Ok(c) if c.kind == Kind::Voip => return self.join_error = "That's a room code. Paste it in the VOIP tab.".into(),
            Ok(c) if game_mode && c.kind != Kind::Game => {
                return self.join_error =
                    "That's a manual port code. Paste it in Manual port forward, or ask for the game code (it starts with kitty:game.).".into()
            }
            Ok(c) => c,
            Err(e) => return self.join_error = e,
        };
        let game = c.game.clone();
        // The port numbers come from the box; an older kitty:port code carries its own.
        let remotes = if ports_input.trim().is_empty() && !c.ports.is_empty() {
            c.ports.clone()
        } else {
            match code::parse_ports(ports_input) {
                Ok(p) => p,
                Err(_) if ports_input.trim().is_empty() => {
                    return self.join_error = "Enter the port number your friend opened, like 25565.".into()
                }
                Err(e) => return self.join_error = e,
            }
        };
        let mut maps = Vec::new();
        for (i, remote) in remotes.iter().enumerate() {
            let mut local = locals.get(i).copied().filter(|p| *p > 0).unwrap_or(*remote);
            if maps.iter().any(|(l, _)| *l == local) {
                return self.join_error = format!("Local port {local} is used twice. Give each one its own number.");
            }
            // A game join picks another port itself; Minecraft finds it through LAN anyway.
            if game_mode && !port_free(local, lan) {
                local = match free_port() {
                    Ok(p) => p,
                    Err(e) => return self.join_error = e,
                };
            }
            if !port_free(local, lan) {
                return self.join_error =
                    format!("Port {local} is already in use on this PC. Pick another local port for {remote}.");
            }
            maps.push((local, *remote));
        }
        let mut args: Vec<String> = vec!["forward".into()];
        if lan {
            args.push("--bind=0.0.0.0".into());
        }
        args.push(c.addr.clone());
        args.extend(maps.iter().map(|(l, r)| format!("{l}:{r}")));
        // A private local port to the host's beacon, for the 30-second check.
        let beacon = (!remotes.contains(&beacon::PORT)).then(free_port).and_then(Result::ok);
        if let Some(b) = beacon {
            args.push(format!("{b}:{}", beacon::PORT));
        }
        match tailcat::spawn(Role::PortJoin, &args, &self.proxy) {
            Ok(p) => {
                let key = self.key();
                let args = args.clone();
                let link = Link { next_ping: Some(Instant::now() + Duration::from_secs(4)), ..Link::default() };
                self.joins.insert(0, PortJoin {
                    key,
                    st: St::Starting,
                    error: String::new(),
                    proc: Some(p),
                    maps,
                    lan,
                    started: Instant::now(),
                    beacon,
                    link,
                    args,
                    peer: peers::new_id(),
                    addr: c.addr,
                    host_name: None,
                    game,
                    worlds: Vec::new(),
                    lan_next: Instant::now() + Duration::from_secs(3),
                    lan_asking: false,
                    log: Log::default(),
                });
                self.set_status("Connecting through TailCat…", "busy");
            }
            Err(e) => self.join_error = e,
        }
    }

    // -------------------------------------------------------------- events

    fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
        // --verbose networking chatter changes nothing; drop it before it costs a redraw.
        if let TcEvent::Line(l) = &ev {
            if tailcat::is_verbose(l) && !tailcat::keep_verbose(l) {
                return false;
            }
        }
        let current = |p: &Option<Proc>| p.as_ref().map(|p| p.id) == Some(id);
        match role {
            Role::ShareHost if current(&self.share.proc) => match ev {
                TcEvent::Line(line) => {
                    if let Some(addr) = tailcat::find_addr(&line).filter(|_| !tailcat::is_verbose(&line)) {
                        self.share.addr = Some(addr);
                        self.share.st = St::Live;
                        self.set_status("Sharing is live. Send the code to your friend.", "ok");
                    }
                    tc_line(&mut self.share.log, &mut self.share.link, line);
                }
                TcEvent::Exited => {
                    let code = self.share.proc.as_mut().and_then(|p| p.exit_code());
                    let why = last_words(&self.share.log);
                    self.stop_share();
                    self.fail_share(format!("TailCat stopped{}. {why}", code.map(|c| format!(" (exit {c})")).unwrap_or_default()));
                }
            },
            Role::RecvForward if current(&self.recv.proc) => match ev {
                TcEvent::Line(line) => tc_line(&mut self.recv.log, &mut self.recv.link, line),
                TcEvent::Exited => {
                    let why = last_words(&self.recv.log);
                    self.fail_recv(format!("The tunnel closed. {why}"));
                }
            },
            Role::PortHost => {
                let Some(h) = self.hosts.iter_mut().find(|h| current(&h.proc)) else { return false };
                match ev {
                    TcEvent::Line(line) => {
                        if let Some(addr) = tailcat::find_addr(&line).filter(|_| !tailcat::is_verbose(&line)) {
                            if h.game.as_deref() == Some("minecraft") {
                                let who = if self.name.is_empty() { "Your friend".to_string() } else { self.name.clone() };
                                lan::serve(&addr, &h.ports, &format!("{who}'s Minecraft server"));
                            }
                            h.addr = Some(addr);
                            h.st = St::Live;
                            self.status = ("Port is open. Send the code and port number to your friend.".into(), "ok");
                        }
                        tc_line(&mut h.log, &mut h.link, line);
                    }
                    TcEvent::Exited => {
                        let why = last_words(&h.log);
                        h.unserve();
                        h.proc = None;
                        h.addr = None;
                        h.st = St::Error;
                        h.error = format!("TailCat stopped. {why}");
                    }
                }
            }
            Role::PortJoin => {
                let Some(j) = self.joins.iter_mut().find(|j| current(&j.proc)) else { return false };
                match ev {
                    TcEvent::Line(line) => {
                        let l = line.to_ascii_lowercase();
                        if j.st == St::Starting && !tailcat::is_verbose(&line) && (l.contains("listening") || l.contains("forwarding")) {
                            j.st = St::Live;
                            self.status = ("Connected. Use the address shown.".into(), "ok");
                        }
                        tc_line(&mut j.log, &mut j.link, line);
                    }
                    TcEvent::Exited => {
                        let why = last_words(&j.log);
                        j.proc = None;
                        j.st = St::Error;
                        j.error = format!("TailCat stopped. {why}");
                    }
                }
            }
            Role::VoipHost | Role::VoipJoin => return self.voip.on_tailcat(role, id, ev),
            _ => return false,
        }
        true
    }

    fn on_share(&mut self, ev: ShareEvent) -> bool {
        match ev {
            ShareEvent::Sending { conn, name, done, total } => {
                let s = self.share.sending.entry(conn).or_insert_with(|| Sending {
                    name: name.clone(),
                    done,
                    total,
                    speed: Speed::new(done),
                });
                s.name = name;
                s.done = done;
                s.total = total;
                s.speed.update(done);
            }
            ShareEvent::Sent { conn, name, ok } => {
                self.share.sending.remove(&conn);
                if ok {
                    self.share.sent += 1;
                    self.share.log.push(format!("KittyConnect: sent {name}"));
                } else {
                    self.share.log.push(format!("KittyConnect: transfer of {name} was interrupted"));
                }
            }
        }
        true
    }

    fn on_recv(&mut self, ev: RecvEvent) -> bool {
        if self.recv.jobs.is_none() {
            return false;
        }
        match ev {
            RecvEvent::List(Ok(files)) => {
                self.recv.st = St::Live;
                // The list arriving proves the tunnel; keep checking every 30 seconds.
                if !self.recv.link.ok_once {
                    self.recv.link.ok_once = true;
                    self.recv.link.peer_seen = true;
                    self.recv.link.next_ping = Some(Instant::now());
                }
                let n = files.len();
                self.recv.files = files;
                self.set_status(format!("Connected. {n} file{} shared with you.", plural(n)), "ok");
            }
            RecvEvent::List(Err(e)) => {
                let why = last_words(&self.recv.log);
                self.fail_recv(if why.is_empty() { e } else { format!("{e} (TailCat: {why})") });
            }
            RecvEvent::Progress { id, done, total } => {
                if let Some(s) = self.recv.states.get_mut(&id) {
                    if s.st != "downloading" {
                        s.speed = Speed::new(done);
                    }
                    s.st = "downloading";
                    s.done = done;
                    s.total = total;
                    s.speed.update(done);
                }
            }
            RecvEvent::Done { id, result } => {
                let name = self.recv.files.iter().find(|f| f.id == id).map(|f| f.name.clone()).unwrap_or_default();
                if let Some(s) = self.recv.states.get_mut(&id) {
                    match result {
                        Ok(path) => {
                            s.st = "done";
                            s.done = s.total;
                            s.path = Some(path);
                            self.status = (format!("Saved {name}"), "ok");
                        }
                        Err(e) => {
                            s.st = "error";
                            s.error = e.clone();
                            self.status = (format!("{name}: {e}"), "bad");
                        }
                    }
                }
            }
        }
        true
    }

    fn on_tick(&mut self) -> bool {
        let mut changed = false;
        // `forward` may say nothing once it's up; a few quiet seconds alive means it is.
        for j in &mut self.joins {
            if j.st == St::Starting && j.started.elapsed() > Duration::from_secs(3) {
                j.st = St::Live;
                changed = true;
            }
        }
        // Whether each host's server has come up, checked in the background.
        self.probe(Instant::now());

        // Everyone gone quiet drops off the lists of who's connected.
        changed |= self.share.peers.expire();
        for h in &mut self.hosts {
            changed |= h.peers.expire();
        }

        // Every 30 seconds, ping the far end through each tunnel we joined,
        // and at once when our path changed, so the host's list stays right.
        let now = Instant::now();
        for link in std::iter::once(&mut self.recv.link).chain(self.joins.iter_mut().map(|j| &mut j.link)) {
            if link.report_due() && !link.pinging {
                link.next_ping = Some(now);
            }
        }
        if self.recv.st == St::Live && self.recv.link.ok_once && self.recv.link.ping_due(now) {
            self.recv.link.ping_started();
            let r = Report { peer: self.recv.peer, addr: &self.recv.addr, path: self.recv.link.path(), rtt: self.recv.link.rtt_ms };
            self.spawn_ping(PingTarget::Recv(self.recv.port), self.recv.port, r.ping());
        }
        let due: Vec<(u64, u16, String)> = self
            .joins
            .iter_mut()
            .filter(|j| j.proc.is_some() && j.link.ping_due(now))
            .filter_map(|j| {
                let b = j.beacon?;
                j.link.ping_started();
                Some((j.key, b, j.report().ping()))
            })
            .collect();
        for (key, port, request) in due {
            self.spawn_ping(PingTarget::Join(key), port, request);
        }
        changed |= self.games_tick(now);
        changed |= self.voip.tick();
        changed
    }

    /// Minecraft: keeps hosts' notes up to date, asks hosts we joined which
    /// LAN worlds they have open, and shows those in this PC's Minecraft.
    fn games_tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        let open = lan::local_worlds();
        for h in self.hosts.iter_mut().filter(|h| h.st == St::Live && h.game.as_deref() == Some("minecraft")) {
            let port = h.ports[0];
            let (note, warn) = if let Some(w) = open.iter().find(|w| w.port == port) {
                (format!("Friends who join see \u{201c}{}\u{201d} under LAN in Multiplayer.", w.motd), String::new())
            } else if let Some(w) = open.first() {
                (String::new(), format!(
                    "Minecraft opened your world on port {}, not {port}. Open to LAN again with Port Number set to {port}, or stop this and host on {}.",
                    w.port, w.port
                ))
            } else if self.listening.get(&port) == Some(&true) {
                (format!("A server is running on port {port}. Friends who join see it under LAN in Multiplayer."), String::new())
            } else {
                (String::new(), format!(
                    "Waiting for Minecraft. In your world press Esc, then Open to LAN, set Port Number to {port}, and press Start LAN World."
                ))
            };
            if h.lan_note != note || h.warn != warn {
                h.lan_note = note;
                h.warn = warn;
                changed = true;
            }
        }

        let due: Vec<(u64, u16, String)> = self
            .joins
            .iter_mut()
            .filter(|j| j.proc.is_some() && j.game.as_deref() == Some("minecraft") && !j.lan_asking && now >= j.lan_next)
            .filter_map(|j| {
                let b = j.beacon?;
                j.lan_asking = true;
                Some((j.key, b, j.addr.clone()))
            })
            .collect();
        for (key, beacon, addr) in due {
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let _ = proxy.send_event(UserEvent::Lan(key, lan::ask(beacon, &addr)));
            });
        }

        // Each world on a port we forward shows at the near end of the tunnel.
        let adverts: Vec<(u16, String)> = self
            .joins
            .iter()
            .filter(|j| j.proc.is_some())
            .flat_map(|j| {
                j.worlds.iter().filter_map(|w| {
                    let local = j.maps.iter().find(|(_, r)| *r == w.port)?.0;
                    let motd = match &j.host_name {
                        Some(n) if !w.motd.contains(n.as_str()) => format!("{} ({n})", w.motd),
                        _ => w.motd.clone(),
                    };
                    Some((local, motd))
                })
            })
            .collect();
        if adverts != self.adverts {
            lan::advertise(adverts.clone());
            self.adverts = adverts;
        }
        changed
    }

    /// Checks in the background whether a server is listening on the ports of
    /// open hosts that need to know: a manual port until its server is up, and
    /// a Minecraft host (which says whether a server is running) all along.
    /// Every 2 seconds, and never more than one check at a time.
    fn probe(&mut self, now: Instant) {
        if self.probing || now < self.probe_at {
            return;
        }
        let mut ports: Vec<u16> = self
            .hosts
            .iter()
            .filter(|h| h.proc.is_some() && (h.game.is_some() || !h.probed || !h.warn.is_empty()))
            .flat_map(|h| h.ports.iter().copied())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        if ports.is_empty() {
            return;
        }
        self.probing = true;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let found = ports.into_iter().map(|p| (p, something_listening(p))).collect();
            let _ = proxy.send_event(UserEvent::Probed(found));
        });
    }

    fn on_probed(&mut self, found: Vec<(u16, bool)>) -> bool {
        self.probing = false;
        self.probe_at = Instant::now() + Duration::from_secs(2);
        self.listening.extend(found);
        let mut changed = false;
        // A manual port warns until something is running on every one of its ports.
        for h in self.hosts.iter_mut().filter(|h| h.proc.is_some() && h.game.is_none()) {
            let idle: Vec<String> = h
                .ports
                .iter()
                .filter(|p| self.listening.get(p) == Some(&false))
                .map(|p| p.to_string())
                .collect();
            if !h.ports.iter().all(|p| self.listening.contains_key(p)) {
                continue;
            }
            let warn = if idle.is_empty() {
                String::new()
            } else {
                format!(
                    "Nothing is running on port {} on this PC yet. Start your server; your friend can connect once it's up.",
                    idle.join(", ")
                )
            };
            h.probed = true;
            if h.warn != warn {
                h.warn = warn;
                changed = true;
            }
        }
        changed
    }

    fn on_lan(&mut self, key: u64, result: Result<Vec<lan::World>, String>) -> bool {
        let Some(j) = self.joins.iter_mut().find(|j| j.key == key) else { return false };
        j.lan_asking = false;
        j.lan_next = Instant::now() + Duration::from_secs(3);
        // A failed ask keeps what we had; the 30-second check notices a host that's gone.
        match result {
            Ok(worlds) if worlds != j.worlds => {
                j.worlds = worlds;
                true
            }
            _ => false,
        }
    }

    fn spawn_ping(&self, target: PingTarget, port: u16, request: String) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = tailcat::ping_port(port, &request, Duration::from_secs(10));
            let _ = proxy.send_event(UserEvent::Ping(target, result));
        });
    }

    fn on_ping(&mut self, target: PingTarget, result: Result<tailcat::Pong, String>) -> bool {
        let name = result.as_ref().ok().and_then(|p| p.name.clone());
        let retry = result.as_ref().is_ok_and(|p| p.retry);
        let result = result.map(|p| p.rtt);
        match target {
            PingTarget::Recv(port) => {
                if self.recv.port != port || self.recv.st != St::Live {
                    return false;
                }
                if name.is_some() {
                    self.recv.host_name = name;
                }
                if self.recv.link.ping_result(result) {
                    self.fail_recv("The sharer stopped sharing or went offline.".into());
                } else if retry && self.recv.link.path() == "relay" {
                    self.recv.log.push("KittyConnect: the sharer asked to retry a direct connection");
                    self.retry_recv();
                }
            }
            PingTarget::Join(key) => {
                let Some(j) = self.joins.iter_mut().find(|j| j.key == key) else { return false };
                if j.proc.is_none() {
                    return false;
                }
                if name.is_some() {
                    j.host_name = name;
                }
                if j.link.ping_result(result) {
                    j.proc = None;
                    j.st = St::Error;
                    j.error = "Your friend closed this port or went offline.".into();
                    self.status = ("A joined port was disconnected by the other side.".into(), "bad");
                }
                // An older host with no beacon never answers; stop asking after a few tries.
                if !j.link.ok_once && j.link.fails >= 3 {
                    j.beacon = None;
                }
                if retry && j.proc.is_some() && j.link.path() == "relay" {
                    j.log.push("KittyConnect: the host asked to retry a direct connection");
                    self.retry_join(key);
                }
            }
        }
        true
    }

    /// Restarts our end of a file-share tunnel to try for a direct path from
    /// scratch. The file list stays; a download in progress fails and can be
    /// started again.
    fn retry_recv(&mut self) {
        if self.recv.st != St::Live || self.recv.args.is_empty() {
            return;
        }
        self.recv.proc = None; // ends the old tunnel
        match tailcat::spawn(Role::RecvForward, &self.recv.args, &self.proxy) {
            Ok(p) => {
                self.recv.proc = Some(p);
                self.recv.link.restarted();
                self.recv.log.push("KittyConnect: reconnecting to try for a direct connection");
                self.set_status("Reconnecting to try for a direct connection…", "busy");
            }
            Err(e) => self.fail_recv(e),
        }
    }

    /// The same for a joined port set. Anything connected through it (a game)
    /// drops for a moment and reconnects to the same local port.
    fn retry_join(&mut self, key: u64) {
        let Some(j) = self.joins.iter_mut().find(|j| j.key == key && j.proc.is_some()) else { return };
        j.proc = None;
        match tailcat::spawn(Role::PortJoin, &j.args, &self.proxy) {
            Ok(p) => {
                j.proc = Some(p);
                j.st = St::Starting;
                j.started = Instant::now();
                j.link.restarted();
                j.log.push("KittyConnect: reconnecting to try for a direct connection");
                self.status = ("Reconnecting to try for a direct connection…".into(), "busy");
            }
            Err(e) => {
                j.st = St::Error;
                j.error = e;
            }
        }
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
            "hostError": self.host_error,
            "joinError": self.join_error,
            "hosts": self.hosts.iter().map(|h| json!({
                "key": h.key,
                "st": h.st.name(),
                "error": h.error,
                "warn": h.warn,
                "ports": h.ports,
                "code": match (&h.game, &h.addr) {
                    (Some(g), Some(a)) => Some(code::game(g, h.ports[0], a)),
                    _ => h.addr.clone(),
                },
                "game": h.game,
                "lanNote": h.lan_note,
                "peers": h.peers.json(),
                "log": h.log.json(),
            })).collect::<Vec<_>>(),
            "joins": self.joins.iter().map(|j| json!({
                "key": j.key,
                "st": j.st.name(),
                "error": j.error,
                "lan": j.lan,
                "maps": j.maps.iter().map(|(l, r)| json!({ "local": l, "remote": r })).collect::<Vec<_>>(),
                "link": j.link.json(),
                "hostName": j.host_name,
                "game": j.game,
                "worlds": j.worlds,
                "checks": j.beacon.is_some(),
                "log": j.log.json(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Tells a port host we're leaving, then closes the tunnel.
fn join_bye(j: &mut PortJoin) -> Option<std::thread::JoinHandle<()>> {
    let port = j.beacon.filter(|_| j.link.ok_once)?;
    let report = j.report().bye();
    let proc = j.proc.take()?;
    Some(std::thread::spawn(move || {
        tailcat::say_bye(port, &report);
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
        "KittyConnect needs the Microsoft Edge WebView2 Runtime, which comes with Windows 11 \
         and most Windows 10 installs, but couldn't start it here.\n\n\
         Press OK to open Microsoft's download page.\n\n({detail})"
    );
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(&text), wide("KittyConnect"));
    let choice = unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OKCANCEL | MB_ICONWARNING) };
    if choice == IDOK {
        open_in_shell("https://developer.microsoft.com/microsoft-edge/webview2/");
    }
}

#[cfg(not(windows))]
fn missing_webview2(detail: &str) {
    eprintln!("KittyConnect couldn't start the system WebView ({detail})");
}
