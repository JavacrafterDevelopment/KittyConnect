//! The in-game overlay: who's talking, and a stream you pin, drawn over your
//! game; a hotkey opens it for changes, with the game dimmed behind.
//!
//! It is Portside's own window: see-through, always on top, ignoring the
//! mouse except while open for changes. Nothing is put into the game, no
//! input hooks are installed, and the game's memory and drawing are never
//! touched, so there is nothing for anti-cheat to object to. The cost of
//! staying out of the game is that it only shows over games in windowed or
//! borderless windowed mode; exclusive full screen draws over everything.
//!
//! The hotkey is the OS's ordinary one (RegisterHotKey on Windows, Carbon's
//! RegisterEventHotKey on macOS), not a keyboard hook. Off by default.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tao::dpi::{PhysicalPosition, PhysicalSize};
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::UserEvent;

const OVERLAY_HTML: &str = include_str!("../ui/overlay.html");
const OVERLAY_JS: &str = include_str!("../ui/overlay.js");

/// The hotkeys on offer: (id, label).
#[cfg(target_os = "macos")]
pub const HOTKEYS: &[(&str, &str)] = &[
    ("shift+grave", "Shift + `"),
    ("ctrl+grave", "Control + `"),
    ("alt+grave", "Option + `"),
    ("ctrl+shift+o", "Control + Shift + O"),
];
#[cfg(not(target_os = "macos"))]
pub const HOTKEYS: &[(&str, &str)] = &[
    ("shift+grave", "Shift + `"),
    ("ctrl+grave", "Ctrl + `"),
    ("alt+grave", "Alt + `"),
    ("ctrl+shift+o", "Ctrl + Shift + O"),
];

/// Whether this OS has the overlay at all.
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    pub hotkey: String,
    /// The list of people in your voice channel.
    pub show_voice: bool,
    /// "always", or "talking": names only while someone talks.
    pub names: String,
    /// The pinned stream's opacity, 0.2 to 1.
    pub stream_opacity: f32,
    /// Keep the overlay out of screen shares and recordings.
    pub hide_from_capture: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            hotkey: "shift+grave".into(),
            show_voice: true,
            names: "always".into(),
            stream_opacity: 1.0,
            hide_from_capture: true,
        }
    }
}

impl Config {
    /// Takes the fields the page sent, keeping the rest; values are clamped.
    pub fn merge(&mut self, v: &Value) {
        if let Some(b) = v["enabled"].as_bool() {
            self.enabled = b;
        }
        if let Some(h) = v["hotkey"].as_str() {
            if HOTKEYS.iter().any(|(id, _)| *id == h) {
                self.hotkey = h.into();
            }
        }
        if let Some(b) = v["showVoice"].as_bool() {
            self.show_voice = b;
        }
        if let Some(n) = v["names"].as_str() {
            if n == "always" || n == "talking" {
                self.names = n.into();
            }
        }
        if let Some(o) = v["streamOpacity"].as_f64() {
            self.stream_opacity = (o as f32).clamp(0.2, 1.0);
        }
        if let Some(b) = v["hideFromCapture"].as_bool() {
            self.hide_from_capture = b;
        }
    }
}

fn settings_path() -> std::path::PathBuf {
    crate::portable::data_dir().join("settings.json")
}

pub fn load() -> Config {
    std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| serde_json::from_value::<Config>(v["overlay"].clone()).ok())
        .unwrap_or_default()
}

fn save(c: &Config) {
    let path = settings_path();
    let mut v = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or(json!({}));
    v["overlay"] = json!(c);
    let tmp = path.with_extension("json.part");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// What the overlay page asked for that the app itself does (mute, watch…).
pub enum FromOverlay {
    Nothing,
    /// Run this command as if the main page sent it.
    App(Value),
    /// The settings changed: tell the main page.
    Config,
}

struct Shown {
    window: Window,
    webview: WebView,
}

pub struct Overlay {
    pub config: Config,
    proxy: EventLoopProxy<UserEvent>,
    shown: Option<Shown>,
    editing: bool,
    pinned: Option<u16>,
    hotkey: Option<hotkey::Hotkey>,
    /// Which hotkey `hotkey` is, so it's only registered again on a change.
    hotkey_id: String,
    hotkey_error: String,
    /// The page in the overlay window said it's running.
    ready: bool,
    /// Last thing that decided whether the window is up.
    in_voice: bool,
    watching: Vec<u16>,
    visible: bool,
    /// The window that had focus before the overlay opened (the game).
    game: platform::Front,
    last_state: Value,
}

impl Overlay {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Overlay {
        Overlay {
            config: load(),
            proxy,
            shown: None,
            editing: false,
            pinned: None,
            hotkey: None,
            hotkey_id: String::new(),
            hotkey_error: String::new(),
            ready: false,
            in_voice: false,
            watching: Vec::new(),
            visible: false,
            game: platform::Front::default(),
            last_state: Value::Null,
        }
    }

    pub fn is_window(&self, id: tao::window::WindowId) -> bool {
        self.shown.as_ref().is_some_and(|s| s.window.id() == id)
    }

    /// For the main page's settings.
    pub fn config_json(&self) -> Value {
        json!({
            "type": "overlayConfig",
            "config": self.config,
            "supported": SUPPORTED,
            "hotkeys": HOTKEYS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect::<Vec<_>>(),
            "hotkeyLabel": HOTKEYS.iter().find(|(id, _)| *id == self.config.hotkey).map(|h| h.1).unwrap_or(""),
            "hotkeyError": self.hotkey_error,
            "pinned": self.pinned,
            "editing": self.editing,
        })
    }

    /// Brings the window and hotkey in line with the settings.
    pub fn apply(&mut self, target: &EventLoopWindowTarget<UserEvent>, ctx: &mut WebContext) {
        let on = self.config.enabled && SUPPORTED;
        if !on {
            self.editing = false;
            self.hotkey = None;
            self.hotkey_id.clear();
            self.hotkey_error.clear();
            if self.shown.take().is_some() {
                self.game.restore();
            }
            self.ready = false;
            self.visible = false;
            return;
        }
        if self.hotkey.is_none() || self.hotkey_id != self.config.hotkey {
            self.hotkey = None;
            let proxy = self.proxy.clone();
            match hotkey::Hotkey::register(&self.config.hotkey, move || {
                let _ = proxy.send_event(UserEvent::OverlayHotkey);
            }) {
                Ok(h) => {
                    self.hotkey = Some(h);
                    self.hotkey_error.clear();
                }
                Err(e) => self.hotkey_error = e,
            }
            self.hotkey_id = self.config.hotkey.clone();
        }
        if self.shown.is_none() {
            match self.build(target, ctx) {
                Ok(s) => self.shown = Some(s),
                Err(e) => {
                    self.hotkey_error = format!("The overlay couldn't open: {e}");
                    return;
                }
            }
        }
        if let Some(s) = &self.shown {
            platform::hide_from_capture(&s.window, self.config.hide_from_capture);
        }
        self.send(json!({ "type": "config", "config": self.config, "hotkey": self.hotkey_label() }));
        self.update_visible();
    }

    fn hotkey_label(&self) -> &'static str {
        HOTKEYS.iter().find(|(id, _)| *id == self.config.hotkey).map(|h| h.1).unwrap_or("")
    }

    fn build(&self, target: &EventLoopWindowTarget<UserEvent>, ctx: &mut WebContext) -> Result<Shown, String> {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut b = WindowBuilder::new()
            .with_title("Portside overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_focusable(false)
            .with_visible_on_all_workspaces(true)
            .with_visible(false);
        #[cfg(windows)]
        {
            use tao::platform::windows::WindowBuilderExtWindows;
            b = b.with_skip_taskbar(true).with_undecorated_shadow(false);
        }
        let window = b.build(target).map_err(|e| e.to_string())?;
        let _ = window.set_ignore_cursor_events(true);
        platform::prepare(&window);

        let proxy = self.proxy.clone();
        let page = OVERLAY_HTML.replace("/*{{JS}}*/", OVERLAY_JS);
        let webview = WebViewBuilder::new_with_web_context(ctx)
            .with_custom_protocol("kco".into(), move |_id, _req| {
                wry::http::Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .header("Cache-Control", "no-store")
                    .body(std::borrow::Cow::Owned(page.as_bytes().to_vec()))
                    .unwrap()
            })
            .with_url("kco://localhost/")
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_devtools(cfg!(debug_assertions))
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::OverlayIpc(req.into_body()));
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        let s = Shown { window, webview };
        fit_to_cursor_monitor(&s.window);
        Ok(s)
    }

    fn send(&self, msg: Value) {
        if let (Some(s), true) = (&self.shown, self.ready) {
            let _ = s.webview.evaluate_script(&format!("window.onHost && window.onHost({msg})"));
        }
    }

    /// The hotkey: open for changes, or close again.
    pub fn toggle_edit(&mut self) {
        let on = !self.editing;
        self.set_editing(on);
    }

    fn set_editing(&mut self, on: bool) {
        let Some(s) = &self.shown else { return };
        if on == self.editing {
            return;
        }
        self.editing = on;
        if on {
            self.game = platform::Front::now();
            fit_to_cursor_monitor(&s.window);
            s.window.set_visible(true);
            let _ = s.window.set_ignore_cursor_events(false);
            s.window.set_focusable(true);
            s.window.set_focus();
            let _ = s.webview.focus();
        } else {
            let _ = s.window.set_ignore_cursor_events(true);
            s.window.set_focusable(false);
            self.game.restore();
        }
        self.visible = on || self.visible;
        self.send(json!({ "type": "edit", "on": on }));
        self.update_visible();
    }

    /// The overlay is only on screen when there's something on it.
    fn update_visible(&mut self) {
        let Some(s) = &self.shown else { return };
        let pinned = self.pinned.is_some_and(|p| self.watching.contains(&p));
        let want = self.editing || (self.config.show_voice && self.in_voice) || pinned;
        if want != self.visible {
            if want {
                fit_to_cursor_monitor(&s.window);
            }
            s.window.set_visible(want);
            self.visible = want;
        }
    }

    pub fn active(&self) -> bool {
        self.shown.is_some()
    }

    /// The room's state, cut down to what the overlay draws.
    pub fn push_state(&mut self, voip: &Value) {
        let watching: Vec<u16> = voip["watching"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64()).map(|v| v as u16).collect()).unwrap_or_default();
        self.in_voice = voip["inVoice"].as_bool().unwrap_or(false) && voip["st"] == "live";
        self.watching = watching;
        if self.pinned.is_some_and(|p| !self.watching.contains(&p)) {
            self.pinned = None;
        }
        let members: Vec<Value> = voip["members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|m| {
                        json!({
                            "id": m["id"], "name": m["name"], "voice": m["voice"], "muted": m["muted"],
                            "deaf": m["deaf"], "smuted": m["smuted"], "channel": m["channel"],
                            "screen": m["screen"].as_object().map(|s| json!({ "codec": s.get("codec"), "w": s.get("w"), "h": s.get("h"), "label": s.get("label") })),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let state = json!({
            "type": "state",
            "live": voip["st"] == "live",
            "room": voip["room"],
            "me": voip["me"],
            "inVoice": self.in_voice,
            "muted": voip["muted"],
            "deaf": voip["deaf"],
            "channel": voip["channel"],
            "channels": voip["channels"],
            "members": members,
            "speaking": voip["speaking"],
            "watching": self.watching,
            "pinned": self.pinned,
        });
        if state != self.last_state {
            self.send(state.clone());
            self.last_state = state;
        }
        self.update_visible();
    }

    /// The frequent messages: who's talking, and the pinned stream's video.
    pub fn forward(&self, live: &Value) {
        if !self.visible {
            return;
        }
        match live["type"].as_str() {
            Some("voipLive") => self.send(live.clone()),
            Some("video") if self.pinned.is_some() && live["from"].as_u64() == self.pinned.map(u64::from) => self.send(live.clone()),
            _ => {}
        }
    }

    /// Pins a stream (or none). Returns the watch command for the app, since a
    /// pinned stream has to be one you're watching.
    pub fn pin(&mut self, id: Option<u16>) -> Option<Value> {
        self.pinned = id;
        self.last_state = Value::Null;
        let cmd = id.filter(|i| !self.watching.contains(i)).map(|i| {
            self.watching.push(i);
            json!({ "cmd": "voip.watch", "id": i, "on": true })
        });
        self.send(json!({ "type": "pinned", "id": id }));
        self.update_visible();
        cmd
    }

    /// From the main page: `overlay.*` commands.
    pub fn on_main(&mut self, msg: &Value, target: &EventLoopWindowTarget<UserEvent>, ctx: &mut WebContext) -> Option<Value> {
        match msg["cmd"].as_str().unwrap_or_default() {
            "overlay.config" => {
                self.config.merge(&msg["config"]);
                save(&self.config);
                self.apply(target, ctx);
                None
            }
            "overlay.open" => {
                self.set_editing(true);
                None
            }
            "overlay.pin" => {
                let id = msg["id"].as_u64().map(|v| v as u16);
                let id = if id == self.pinned { None } else { id };
                self.pin(id)
            }
            "overlay.reset" => {
                self.send(json!({ "type": "resetLayout" }));
                None
            }
            _ => None,
        }
    }

    /// From the overlay window's page.
    pub fn on_overlay(&mut self, msg: &Value) -> FromOverlay {
        let cmd = msg["cmd"].as_str().unwrap_or_default();
        match cmd {
            "ready" => {
                self.ready = true;
                self.last_state = Value::Null;
                self.send(json!({ "type": "config", "config": self.config, "hotkey": self.hotkey_label() }));
                self.send(json!({ "type": "edit", "on": self.editing }));
                FromOverlay::Config
            }
            "close" => {
                self.set_editing(false);
                FromOverlay::Nothing
            }
            "pin" => {
                let id = msg["id"].as_u64().map(|v| v as u16);
                match self.pin(id) {
                    Some(c) => FromOverlay::App(c),
                    None => FromOverlay::Config,
                }
            }
            "set" => {
                self.config.merge(&msg["config"]);
                save(&self.config);
                self.send(json!({ "type": "config", "config": self.config, "hotkey": self.hotkey_label() }));
                self.update_visible();
                FromOverlay::Config
            }
            // The few room controls the overlay has.
            "voip.mute" | "voip.keyframe" | "voip.shareVolume" | "voip.voice" => FromOverlay::App(msg.clone()),
            _ => FromOverlay::Nothing,
        }
    }

    pub fn close_edit(&mut self) {
        self.set_editing(false);
    }
}

/// Covers the whole monitor the mouse is on (where the game is, nearly always).
fn fit_to_cursor_monitor(w: &Window) {
    let monitor = w
        .cursor_position()
        .ok()
        .and_then(|p| w.monitor_from_point(p.x, p.y))
        .or_else(|| w.primary_monitor())
        .or_else(|| w.current_monitor());
    if let Some(m) = monitor {
        let PhysicalPosition { x, y } = m.position();
        let PhysicalSize { width, height } = m.size();
        w.set_outer_position(PhysicalPosition::new(x, y));
        w.set_inner_size(PhysicalSize::new(width, height));
    }
}

// ------------------------------------------------------------- per OS

#[cfg(windows)]
mod platform {
    use tao::platform::windows::WindowExtWindows;
    use tao::window::Window;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsWindow, SetForegroundWindow, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    #[derive(Default)]
    pub struct Front(isize);

    impl Front {
        pub fn now() -> Front {
            Front(unsafe { GetForegroundWindow() } as isize)
        }
        /// Gives focus back to the game.
        pub fn restore(&mut self) {
            let h = std::mem::take(&mut self.0);
            if h != 0 && unsafe { IsWindow(h as _) } != 0 {
                unsafe { SetForegroundWindow(h as _) };
            }
        }
    }

    pub fn prepare(_w: &Window) {}

    /// Leaves the overlay out of screen shares, recordings and screenshots
    /// (Windows 10 2004 and later; older ones ignore it).
    pub fn hide_from_capture(w: &Window, on: bool) {
        unsafe { SetWindowDisplayAffinity(w.hwnd() as _, if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE }) };
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use tao::platform::macos::WindowExtMacOS;
    use tao::window::Window;

    #[derive(Default)]
    pub struct Front(Option<Retained<AnyObject>>);

    impl Front {
        pub fn now() -> Front {
            let Some(ws) = AnyClass::get(c"NSWorkspace") else { return Front(None) };
            unsafe {
                let shared: *mut AnyObject = msg_send![ws, sharedWorkspace];
                if shared.is_null() {
                    return Front(None);
                }
                let app: *mut AnyObject = msg_send![&*shared, frontmostApplication];
                Front(Retained::retain(app))
            }
        }
        pub fn restore(&mut self) {
            if let Some(app) = self.0.take() {
                // NSApplicationActivateIgnoringOtherApps
                let _: bool = unsafe { msg_send![&*app, activateWithOptions: 2usize] };
            }
        }
    }

    /// Shows over full-screen apps and on every Space.
    pub fn prepare(w: &Window) {
        let ns = w.ns_window() as *mut AnyObject;
        if ns.is_null() {
            return;
        }
        unsafe {
            let ns = &*ns;
            let b: usize = msg_send![ns, collectionBehavior];
            // canJoinAllSpaces | stationary | ignoresCycle | fullScreenAuxiliary
            let _: () = msg_send![ns, setCollectionBehavior: b | 1 | (1 << 4) | (1 << 6) | (1 << 8)];
            // NSStatusWindowLevel, above other apps' floating windows.
            let _: () = msg_send![ns, setLevel: 25isize];
            let _: () = msg_send![ns, setHasShadow: false];
        }
    }

    /// NSWindowSharingNone keeps it out of screen capture.
    pub fn hide_from_capture(w: &Window, on: bool) {
        let ns = w.ns_window() as *mut AnyObject;
        if ns.is_null() {
            return;
        }
        let _: () = unsafe { msg_send![&*ns, setSharingType: if on { 0usize } else { 1usize }] };
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use tao::window::Window;

    #[derive(Default)]
    pub struct Front;
    impl Front {
        pub fn now() -> Front {
            Front
        }
        pub fn restore(&mut self) {}
    }
    pub fn prepare(_w: &Window) {}
    pub fn hide_from_capture(_w: &Window, _on: bool) {}
}

// ------------------------------------------------------------- hotkeys

/// "shift+grave" → which modifiers and which key.
#[cfg_attr(not(any(windows, target_os = "macos", test)), allow(dead_code))]
fn parse_hotkey(id: &str) -> Option<(bool, bool, bool, char)> {
    let (mut shift, mut ctrl, mut alt, mut key) = (false, false, false, None);
    for part in id.split('+') {
        match part {
            "shift" => shift = true,
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "grave" => key = Some('`'),
            k if k.len() == 1 => key = k.chars().next().map(|c| c.to_ascii_uppercase()),
            _ => return None,
        }
    }
    Some((shift, ctrl, alt, key?))
}

#[cfg(windows)]
mod hotkey {
    use std::sync::mpsc::channel;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_OEM_3,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT};

    /// A system-wide hotkey, on a thread of its own with its own message queue.
    pub struct Hotkey {
        thread: u32,
    }

    impl Hotkey {
        pub fn register(id: &str, fire: impl Fn() + Send + 'static) -> Result<Hotkey, String> {
            let (shift, ctrl, alt, key) = super::parse_hotkey(id).ok_or("Unknown hotkey.")?;
            let vk = if key == '`' { VK_OEM_3 as u32 } else { key as u32 };
            let mods = MOD_NOREPEAT | if shift { MOD_SHIFT } else { 0 } | if ctrl { MOD_CONTROL } else { 0 } | if alt { MOD_ALT } else { 0 };
            let (tx, rx) = channel::<Result<u32, String>>();
            std::thread::spawn(move || unsafe {
                let thread = GetCurrentThreadId();
                if RegisterHotKey(std::ptr::null_mut(), 1, mods, vk) == 0 {
                    let _ = tx.send(Err("That hotkey is taken by another program. Pick another one.".into()));
                    return;
                }
                let _ = tx.send(Ok(thread));
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    if msg.message == WM_HOTKEY {
                        fire();
                    }
                }
                UnregisterHotKey(std::ptr::null_mut(), 1);
            });
            let thread = rx.recv().map_err(|_| "The hotkey couldn't be set.".to_string())??;
            Ok(Hotkey { thread })
        }
    }

    impl Drop for Hotkey {
        fn drop(&mut self) {
            unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
        }
    }
}

#[cfg(target_os = "macos")]
mod hotkey {
    //! Carbon's hot keys: the standard way for an app to get a system-wide
    //! key, with no Accessibility permission and no event tap.
    use std::ffi::c_void;
    use std::sync::Mutex;

    #[repr(C)]
    struct EventTypeSpec {
        class: u32,
        kind: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }
    type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn GetApplicationEventTarget() -> *mut c_void;
        fn InstallEventHandler(target: *mut c_void, handler: Handler, count: usize, list: *const EventTypeSpec, data: *mut c_void, out: *mut *mut c_void) -> i32;
        fn RegisterEventHotKey(code: u32, mods: u32, id: EventHotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
        fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    }

    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;

    type Fire = Box<dyn Fn() + Send>;
    static FIRE: Mutex<Option<Fire>> = Mutex::new(None);
    static INSTALLED: std::sync::Once = std::sync::Once::new();

    extern "C" fn pressed(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
        if let Ok(f) = FIRE.lock() {
            if let Some(f) = f.as_ref() {
                f();
            }
        }
        0
    }

    pub struct Hotkey {
        handle: *mut c_void,
    }

    impl Hotkey {
        /// Call on the main thread (the event loop's).
        pub fn register(id: &str, fire: impl Fn() + Send + 'static) -> Result<Hotkey, String> {
            let (shift, ctrl, alt, key) = super::parse_hotkey(id).ok_or("Unknown hotkey.")?;
            // kVK_ANSI_Grave, kVK_ANSI_O
            let code = match key {
                '`' => 0x32,
                'O' => 0x1F,
                _ => return Err("Unknown hotkey.".into()),
            };
            let mods = if shift { 0x200 } else { 0 } | if ctrl { 0x1000 } else { 0 } | if alt { 0x800 } else { 0 };
            *FIRE.lock().unwrap() = Some(Box::new(fire));
            unsafe {
                INSTALLED.call_once(|| {
                    let spec = EventTypeSpec { class: K_EVENT_CLASS_KEYBOARD, kind: K_EVENT_HOT_KEY_PRESSED };
                    let mut h = std::ptr::null_mut();
                    InstallEventHandler(GetApplicationEventTarget(), pressed, 1, &spec, std::ptr::null_mut(), &mut h);
                });
                let mut handle = std::ptr::null_mut();
                let hid = EventHotKeyId { signature: u32::from_be_bytes(*b"PSov"), id: 1 };
                if RegisterEventHotKey(code, mods, hid, GetApplicationEventTarget(), 0, &mut handle) != 0 || handle.is_null() {
                    return Err("That hotkey is taken by another app. Pick another one.".into());
                }
                Ok(Hotkey { handle })
            }
        }
    }

    impl Drop for Hotkey {
        fn drop(&mut self) {
            unsafe { UnregisterEventHotKey(self.handle) };
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod hotkey {
    pub struct Hotkey;
    impl Hotkey {
        pub fn register(_id: &str, _fire: impl Fn() + Send + 'static) -> Result<Hotkey, String> {
            Err("The overlay isn't available on this system.".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_parse_and_settings_merge_safely() {
        for (id, _) in HOTKEYS {
            assert!(parse_hotkey(id).is_some(), "{id}");
        }
        assert_eq!(parse_hotkey("shift+grave"), Some((true, false, false, '`')));
        assert_eq!(parse_hotkey("ctrl+shift+o"), Some((true, true, false, 'O')));
        assert_eq!(parse_hotkey("win+grave"), None);

        let mut c = Config::default();
        assert!(!c.enabled, "off until turned on");
        c.merge(&json!({ "enabled": true, "hotkey": "nonsense", "names": "loud", "streamOpacity": 5.0 }));
        assert!(c.enabled);
        assert_eq!(c.hotkey, "shift+grave");
        assert_eq!(c.names, "always");
        assert_eq!(c.stream_opacity, 1.0);
        c.merge(&json!({ "hotkey": "alt+grave", "names": "talking", "streamOpacity": 0.1, "showVoice": false }));
        assert_eq!((c.hotkey.as_str(), c.names.as_str(), c.stream_opacity, c.show_voice), ("alt+grave", "talking", 0.2, false));
        let back: Config = serde_json::from_value(json!(c)).unwrap();
        assert_eq!(back, c);
        // Old settings files without the overlay give the defaults.
        let none: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(none, Config::default());
    }
}
