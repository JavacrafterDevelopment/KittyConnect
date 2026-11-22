//! The Hub tab in the app: hosting, joining, saved hubs and invites.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Off,
    Hosting,
    Joined,
}

/// An invite we got, until it's joined or dismissed.
pub(super) struct Invite {
    pub(super) id: u64,
    pub(super) from: String,
    pub(super) kind: &'static str,
    pub(super) code: String,
    pub(super) title: String,
    pub(super) at: Instant,
}

/// The most invites kept; older ones go.
pub(super) const MAX_INVITES: usize = 20;

/// A delete waiting to hear back from the hub before it finishes.
pub(super) struct Finish {
    pub(super) id: String,
    pub(super) until: Instant,
    pub(super) everyone: bool,
}

/// The Hub tab: the hub we host or joined (one at a time), who's in it,
/// invites we got, and the hubs saved on this PC.
pub struct Hub {
    /// Tells this app's background results from another's (the end-to-end
    /// check runs two apps in one process).
    pub tag: u64,
    pub(super) gen: u64,
    pub(super) mode: Mode,
    /// "off" | "starting" | "live" | "ended"
    pub(super) st: &'static str,
    pub error: String,
    pub(super) notice: String,
    pub(super) name: String,
    /// The hub's id, and when it's deleted if it's a saved one (our clock).
    pub(super) id: String,
    pub(super) expires: Option<u64>,
    pub(super) server: Option<HubServer>,
    pub(super) proc: Option<Proc>,
    pub(super) conn: Option<HubConn>,
    pub(super) code: Option<String>,
    pub(super) me: u64,
    pub(super) members: Vec<Value>,
    pub(super) invites: Vec<Invite>,
    pub(super) next_invite: u64,
    pub link: Link,
    pub(super) log: Log,
    /// The path and ping we last told the hub about.
    pub(super) reported: &'static str,
    pub(super) reported_rtt: Option<u32>,
    /// What the last invite we sent came to.
    pub(super) sent: String,
    pub(super) reconnecting: bool,
    /// Hubs saved on this PC, and where.
    pub saved: Vec<Saved>,
    pub store: Store,
    /// Which of them this session is (we host it, or joined it and saved it).
    pub(super) saved_id: Option<String>,
    pub(super) book: Vec<Entry>,
    pub(super) gone: Vec<Gone>,
    /// What others have open in the hub.
    pub(super) offers: Vec<Value>,
    /// "Open to hub": everything we open is shown to everyone in it.
    pub(super) open_to_hub: bool,
    /// What we've told the hub we have open: (id, code).
    pub(super) offered: Vec<(String, String)>,
    /// A key being made for a saved hub: "create" or "save".
    pub(super) making: Option<&'static str>,
    pub(super) scans: HashMap<String, Scan>,
    /// Open found the hub already open somewhere: which hub, and where.
    pub(super) prompt: Option<(String, Vec<Hit>)>,
    pub(super) finish: Option<Finish>,
    /// To act on our own when the hub moves or its host vanishes: the event
    /// loop and our name, from the last start or join.
    pub(super) proxy: Option<EventLoopProxy<UserEvent>>,
    pub(super) me_name: String,
    /// We lost a saved hub's host (reconnecting since).
    pub(super) lost_since: Option<Instant>,
    /// Who takes over a saved hub whose host vanished, in order (everyone's
    /// app works out the same list from the last member list).
    pub(super) failover: Option<(String, Vec<Entry>)>,
    /// Scan a hub again soon (we handed ours over, so someone else has it now).
    pub(super) rescan: Option<(String, Instant)>,
    /// While we host a saved hub: when we next look for another open copy of
    /// it, and how many looks so far.
    pub(super) watch: Option<(Instant, u32)>,
    /// When our last such look started.
    pub(super) last_look: Option<Instant>,
    pub(super) scan_no: u64,
}

/// How long a saved hub's host can be gone before someone else takes over.
pub(super) const TAKE_OVER_AFTER: Duration = Duration::from_secs(15);
/// How long we wait for the copy we went to (whoever's next in line) before
/// going to the one after: they notice the host is gone a few seconds apart,
/// and their copy takes a moment to open.
pub(super) const NEXT_AFTER: Duration = Duration::from_secs(45);

impl Default for Hub {
    fn default() -> Self {
        Hub {
            tag: crate::peers::new_id(),
            gen: 0,
            mode: Mode::Off,
            st: "off",
            error: String::new(),
            notice: String::new(),
            name: String::new(),
            id: String::new(),
            expires: None,
            server: None,
            proc: None,
            conn: None,
            code: None,
            me: 0,
            members: Vec::new(),
            invites: Vec::new(),
            next_invite: 1,
            link: Link::default(),
            log: Log::default(),
            reported: "unknown",
            reported_rtt: None,
            sent: String::new(),
            reconnecting: false,
            saved: Vec::new(),
            store: Store::default(),
            saved_id: None,
            book: Vec::new(),
            gone: Vec::new(),
            offers: Vec::new(),
            open_to_hub: false,
            offered: Vec::new(),
            making: None,
            scans: HashMap::new(),
            prompt: None,
            finish: None,
            proxy: None,
            me_name: String::new(),
            lost_since: None,
            failover: None,
            rescan: None,
            watch: None,
            last_look: None,
            scan_no: 0,
        }
    }
}

/// What the page needs to know to join an invite.
pub struct Accepted {
    pub kind: &'static str,
    pub code: String,
}

pub(super) type Status = Option<(String, &'static str)>;

impl Hub {
    /// The Hub tab, with the hubs saved in `store`.
    pub fn with_store(store: Store) -> Hub {
        Hub { saved: store.load_all(), store, ..Hub::default() }
    }

    pub fn is_live(&self) -> bool {
        self.st == "live"
    }

    pub fn is_on(&self) -> bool {
        self.mode != Mode::Off
    }

    pub fn is_host(&self) -> bool {
        self.mode == Mode::Hosting
    }

    pub(super) fn reset(&mut self) {
        self.conn = None; // says bye
        // A hub we joined hears our bye through the tunnel, so our TailCat
        // stays a moment for it (closed at once, the bye was often lost and
        // we stayed listed there until the silence ran out). Its key is a
        // new one each time, so the next TailCat doesn't clash with it.
        match (self.mode, self.proc.take()) {
            (Mode::Joined, Some(p)) => {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    drop(p);
                });
            }
            (_, p) => drop(p),
        }
        self.server = None;
        self.mode = Mode::Off;
        self.code = None;
        self.me = 0;
        self.members.clear();
        self.link = Link::default();
        self.reported = "unknown";
        self.reported_rtt = None;
        self.sent.clear();
        self.reconnecting = false;
        self.id.clear();
        self.expires = None;
        self.saved_id = None;
        self.book.clear();
        self.gone.clear();
        self.offers.clear();
        self.offered.clear();
        self.finish = None;
        self.lost_since = None;
        self.watch = None;
        self.gen += 1;
    }

    /// Before we host or join a hub: whatever scans are still going are no
    /// use any more, so they end.
    pub(super) fn begin(&mut self) {
        self.reset();
        self.stop_scans(None);
        self.error.clear();
        self.notice.clear();
        self.prompt = None;
        self.log = Log::default();
    }

    /// Starts a temporary hub on this PC (a new key, gone when it closes) and joins it.
    pub fn host_temporary(&mut self, hub_name: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.begin();
        let Some((me, name)) = self.names(hub_name, my_name) else { return };
        let meta = Meta { id: hubstore::new_id(), name, expires: None };
        self.start(meta, None, None, &me, proxy);
    }

    /// Our name and the hub's ("<name>'s hub" when none is given).
    pub(super) fn names(&mut self, hub_name: &str, my_name: &str) -> Option<(String, String)> {
        let me = clean(my_name, crate::peers::MAX_NAME);
        if me.is_empty() {
            self.error = "Pick your name first (top right).".into();
            return None;
        }
        let name = clean(hub_name, 48);
        Some((me.clone(), if name.is_empty() { format!("{me}'s hub") } else { name }))
    }

    /// The hub's server, served with the official TailCat (its one port and
    /// nothing else): a new key, or a saved hub's own.
    pub(super) fn start(&mut self, meta: Meta, port: Option<u16>, key: Option<std::path::PathBuf>, me: &str, proxy: &EventLoopProxy<UserEvent>) -> bool {
        let server = match HubServer::start(meta.clone(), port) {
            Ok(s) => s,
            Err(e) => {
                self.error = e;
                return false;
            }
        };
        let key = key.map(|k| format!("--key={}", k.display())).unwrap_or_else(|| "--key=new".into());
        let args = vec!["serve".into(), key, server.port.to_string()];
        match tailcat::spawn(Role::HubHost, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => {
                self.error = e;
                return false;
            }
        }
        let hello = json!({ "op": "hello", "v": PROTOCOL, "peer": crate::peers::new_id(), "name": me, "host": server.host_token });
        self.conn = Some(connect(server.port, hello, to_app(self.gen, proxy)));
        self.proxy = Some(proxy.clone());
        self.me_name = me.to_string();
        self.name = meta.name;
        self.id = meta.id;
        self.expires = meta.expires;
        self.server = Some(server);
        self.mode = Mode::Hosting;
        self.st = "starting";
        true
    }

    /// Makes a saved hub (its key takes a few seconds), then opens it.
    pub fn create_saved(&mut self, hub_name: &str, secs: u64, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        if self.making.is_some() {
            return;
        }
        self.begin();
        let Some((_, name)) = self.names(hub_name, my_name) else { return };
        let saved = Saved {
            id: hubstore::new_id(),
            name,
            expires: hubstore::now() + hubstore::clamp_life(secs),
            port: 0,
            code: String::new(),
            book: Vec::new(),
            gone: Vec::new(),
            updated: 0,
        };
        self.make_key(saved, true, proxy, "create");
    }

    /// "Save this hub?": keeps the hub we joined on this PC too, until it
    /// expires for everyone, with our own key and code for it, which goes into
    /// everyone's address book.
    pub fn save_joined(&mut self, proxy: &EventLoopProxy<UserEvent>) {
        let Some(expires) = self.expires.filter(|_| self.mode == Mode::Joined && self.is_live()) else { return };
        if self.making.is_some() || self.saved_id.is_some() || !hubstore::is_id(&self.id) {
            return;
        }
        let left = expires.saturating_sub(hubstore::now());
        if left < 60 {
            return self.error = "This hub is about to expire.".into();
        }
        let saved = Saved {
            id: self.id.clone(),
            name: self.name.clone(),
            expires: hubstore::now() + left.min(hubstore::MAX_LIFE),
            port: 0,
            code: String::new(),
            book: self.book.clone(),
            gone: self.gone.clone(),
            updated: 0,
        };
        self.make_key(saved, false, proxy, "save");
    }

    /// A port and a key (the official `tailcat genkey`), off the UI thread.
    pub(super) fn make_key(&mut self, mut saved: Saved, open_after: bool, proxy: &EventLoopProxy<UserEvent>, what: &'static str) {
        saved.port = match hubstore::pick_port() {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let key = match self.store.prepare(&saved.id) {
            Ok(k) => k,
            Err(e) => return self.error = e,
        };
        self.making = Some(what);
        let (proxy, tag) = (proxy.clone(), self.tag);
        std::thread::spawn(move || {
            let result = tailcat::genkey(&key);
            let _ = proxy.send_event(UserEvent::HubKey(tag, saved, result, open_after));
        });
    }

    /// A saved hub's key is made: keep it, then open it (one we made) or
    /// share our code with the hub we're in (one we saved).
    pub fn on_key(&mut self, mut saved: Saved, result: Result<String, String>, open_after: bool, my_name: &str, proxy: &EventLoopProxy<UserEvent>) -> Status {
        self.making = None;
        let addr = match result {
            Ok(a) => a,
            Err(e) => {
                self.store.delete(&saved.id);
                self.error = e;
                return None;
            }
        };
        saved.code = code::hub(saved.port, &addr);
        let me = clean(my_name, crate::peers::MAX_NAME);
        let mine = Entry { name: me, code: saved.code.clone(), at: hubstore::now() };
        hubstore::merge(&mut saved.book, &mut saved.gone, &[mine], &[]);
        if let Err(e) = self.store.store(&saved) {
            self.store.delete(&saved.id);
            self.error = e;
            return None;
        }
        let (id, name) = (saved.id.clone(), saved.name.clone());
        self.saved.retain(|s| s.id != id);
        self.saved.push(saved);
        self.saved.sort_by_key(|s| s.expires);
        if open_after {
            self.open_saved(&id, my_name, proxy);
            return Some((format!("Opening {name}…"), "busy"));
        }
        if self.mode == Mode::Joined && self.id == id {
            self.saved_id = Some(id);
            self.share_address();
            return Some((format!("Saved {name}. You can open it yourself now, and scan for whoever has it open."), "ok"));
        }
        None
    }

    /// Opens our copy of a saved hub, under our same code as always.
    pub fn open_saved(&mut self, id: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        let Some(s) = self.saved.iter().find(|s| s.id == id).cloned() else { return };
        self.begin();
        let Some((me, _)) = self.names(&s.name, my_name) else { return };
        let Some(key) = self.store.key_path(&s.id) else { return };
        let meta = Meta { id: s.id.clone(), name: s.name.clone(), expires: Some(s.expires) };
        if self.start(meta, Some(s.port), Some(key), &me, proxy) {
            self.saved_id = Some(s.id);
        }
    }

    /// Joins a hub (a friend's, or someone's copy of a saved one) through the
    /// official TailCat.
    pub fn join(&mut self, input: &str, my_name: &str, proxy: &EventLoopProxy<UserEvent>) {
        self.begin();
        let me = clean(my_name, crate::peers::MAX_NAME);
        if me.is_empty() {
            return self.error = "Pick your name first (top right).".into();
        }
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::Hub => c,
            Ok(_) => return self.error = "That isn't a hub code. Ask for the code from the Hub tab (it starts with kitty:hub:).".into(),
            Err(e) => return self.error = e,
        };
        let local = match crate::free_port() {
            Ok(p) => p,
            Err(e) => return self.error = e,
        };
        let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        match tailcat::spawn(Role::HubJoin, &args, proxy) {
            Ok(p) => self.proc = Some(p),
            Err(e) => return self.error = e,
        }
        let hello = json!({ "op": "hello", "v": PROTOCOL, "peer": crate::peers::new_id(), "name": me, "path": "unknown" });
        self.conn = Some(connect(local, hello, to_app(self.gen, proxy)));
        self.proxy = Some(proxy.clone());
        self.me_name = me.clone();
        self.code = Some(code::hub(c.ports[0], &c.addr));
        self.mode = Mode::Joined;
        self.st = "starting";
    }

    /// Our code and our copy of the address book, to the hub we're in.
    pub(super) fn share_address(&self) {
        let (Some(c), Some(s)) = (&self.conn, self.saved_id.as_ref().and_then(|id| self.saved.iter().find(|s| s.id == *id))) else { return };
        c.send(json!({ "op": "address", "code": s.code }));
        c.send(json!({ "op": "merge", "book": s.book, "gone": hubstore::gone_codes(&s.gone), "goneAt": s.gone }));
    }

    /// Cancel on "It's already online": the looking stops, and the tile is
    /// as it was.
    pub fn dismiss_prompt(&mut self) {
        if let Some((id, _)) = self.prompt.take() {
            self.stop_scans(Some(&id));
            self.scans.remove(&id);
        }
    }

    /// Deletes a saved hub: just ours (our code leaves everyone's address
    /// book, if we're in the hub now), or for everyone in it right now
    /// (anyone in it can).
    pub fn delete(&mut self, id: &str, everyone: bool) -> Status {
        let here = self.is_live() && self.id == id;
        if everyone {
            let Some(c) = self.conn.as_ref().filter(|_| here) else {
                self.error = "Open or join the hub first: deleting it for everyone reaches those in it.".into();
                return None;
            };
            c.send(json!({ "op": "delete" }));
            self.finish = Some(Finish { id: id.to_string(), until: Instant::now() + Duration::from_secs(3), everyone: true });
            return Some(("Deleting it for everyone…".into(), "busy"));
        }
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
        if let (true, Some(c), Some(code)) = (here, &self.conn, mine) {
            c.send(json!({ "op": "forget", "code": code }));
            self.finish = Some(Finish { id: id.to_string(), until: Instant::now() + Duration::from_secs(2), everyone: false });
            return Some(("Deleting…".into(), "busy"));
        }
        self.finish_delete(id, None)
    }

    /// Our copy goes, key and all; if we're in it, we leave (or close it).
    pub(super) fn finish_delete(&mut self, id: &str, by: Option<String>) -> Status {
        let name = self.saved.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_else(|| self.name.clone());
        let everyone = self.finish.as_ref().is_some_and(|f| f.everyone);
        self.store.delete(id);
        self.saved.retain(|s| s.id != id);
        self.scans.remove(id);
        if self.prompt.as_ref().is_some_and(|(p, _)| p == id) {
            self.prompt = None;
        }
        self.finish = None;
        if self.is_on() && self.id == id {
            // Just ours deleted: the others keep it (handed over). For
            // everyone: nobody does.
            self.close(!everyone && by.is_none());
        }
        let msg = match by {
            Some(by) if !everyone => format!("{by} deleted {name} for everyone."),
            _ if everyone => format!("Deleted {name} for everyone in it."),
            _ => format!("Deleted {name}."),
        };
        self.notice = msg.clone();
        Some((msg, "idle"))
    }

    /// Leaves the hub, or closes it if it's ours (everyone in it is
    /// disconnected; a saved hub stays saved).
    pub fn leave(&mut self) -> bool {
        self.close(true)
    }

    /// Leaves or closes. A saved hub we host is handed over first (if
    /// `hand_over` and someone in it saved it): they keep it open, with
    /// everyone else. True if it was.
    pub(super) fn close(&mut self, hand_over: bool) -> bool {
        let handed = if hand_over { self.hand_over() } else { None };
        let id = self.id.clone();
        self.reset();
        self.failover = None;
        self.st = "off";
        self.error.clear();
        self.notice.clear();
        let Some(name) = handed else { return false };
        self.notice = format!("{name} has it open now, with everyone who was in it. Scan finds it.");
        // A moment for them to open it, then look for it.
        self.rescan = Some((id, Instant::now() + Duration::from_secs(6)));
        true
    }

    pub(super) fn hand_over(&mut self) -> Option<String> {
        if self.mode != Mode::Hosting || self.expires.is_none() {
            return None;
        }
        let name = self.server.as_ref()?.hand_over()?;
        // Our copy stays up a moment longer, so everyone hears where to go.
        let (server, proc) = (self.server.take(), self.proc.take());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            drop(server);
            drop(proc);
        });
        Some(name)
    }

    pub(super) fn end(&mut self, why: &str) {
        self.reset();
        self.st = "ended";
        self.notice = why.to_string();
    }

    pub fn rename(&mut self, name: &str) {
        if let Some(c) = &self.conn {
            c.send(json!({ "op": "name", "name": name }));
        }
    }

    /// Invites people in the hub (`to` empty: everyone) to what `code` opens.
    pub fn invite(&mut self, code_text: &str, title: &str, to: Vec<u64>) -> Result<(), String> {
        let Some(c) = self.conn.as_ref().filter(|_| self.is_live()) else {
            return Err("Join or start a hub first.".into());
        };
        if invitation(code_text).is_none() {
            return Err("That can't be shared in a hub.".into());
        }
        self.sent = "Sending…".into();
        c.send(json!({ "op": "invite", "to": to, "code": code_text, "title": clean(title, MAX_TITLE) }));
        Ok(())
    }

    /// "Open to hub" on or off.
    pub fn set_open(&mut self, on: bool) {
        self.open_to_hub = on;
    }

    /// What we have open now, as (id, title, code): with "Open to hub" on,
    /// everyone in the hub sees it, and stops seeing each thing once it closes.
    pub fn sync_offers(&mut self, open: Vec<(String, String, String)>) {
        let Some(c) = self.conn.as_ref().filter(|_| self.is_live()) else {
            self.offered.clear();
            return;
        };
        let want = if self.open_to_hub { open } else { Vec::new() };
        for (oid, code) in &self.offered {
            if !want.iter().any(|(o, _, c2)| o == oid && c2 == code) {
                c.send(json!({ "op": "withdraw", "oid": oid }));
            }
        }
        for (oid, title, code) in &want {
            if !self.offered.iter().any(|(o, c2)| o == oid && c2 == code) {
                c.send(json!({ "op": "offer", "oid": oid, "title": title, "code": code }));
            }
        }
        self.offered = want.into_iter().map(|(o, _, c)| (o, c)).collect();
    }

    /// Joins something someone has open in the hub.
    pub fn accept_offer(&self, from: u64, oid: &str) -> Option<Accepted> {
        let o = self.offers.iter().find(|o| o["from"].as_u64() == Some(from) && o["oid"] == oid)?;
        let (kind, code) = invitation(o["code"].as_str()?)?;
        Some(Accepted { kind, code })
    }

    /// Host only: takes someone out of the hub.
    pub fn kick(&self, id: u64) {
        if let (Some(c), true) = (&self.conn, self.is_host()) {
            c.send(json!({ "op": "kick", "id": id }));
        }
    }

    /// Takes an invite off the list to join it.
    pub fn accept(&mut self, id: u64) -> Option<Accepted> {
        let i = self.invites.iter().position(|v| v.id == id)?;
        let inv = self.invites.remove(i);
        Some(Accepted { kind: inv.kind, code: inv.code })
    }

    pub fn dismiss(&mut self, id: u64) {
        self.invites.retain(|v| v.id != id);
    }

    /// For the Connections list: how many others are in the hub.
    pub fn others(&self) -> Vec<&Value> {
        self.members.iter().filter(|m| m["id"].as_u64() != Some(self.me)).collect()
    }

    pub fn title(&self) -> &str {
        &self.name
    }

    /// A saved hub (closing it keeps it), rather than a temporary one.
    pub fn is_saved(&self) -> bool {
        self.expires.is_some()
    }

    /// For the end-to-end check: our copy dies without a word (as if the
    /// app crashed), no hand-over.
    #[cfg(any(all(test, target_os = "linux"), feature = "e2e"))]
    pub fn test_crash(&mut self) {
        self.close(false);
    }

    /// For the end-to-end check: how many of hub `id`'s scan's looks still
    /// have their TailCat running.
    #[cfg(any(all(test, target_os = "linux"), feature = "e2e"))]
    pub fn test_looks_running(&self, id: &str) -> usize {
        self.scans.get(id).map_or(0, |sc| sc.stop.running.lock().unwrap().iter().filter(|p| p.lock().unwrap().is_some()).count())
    }

    /// For the end-to-end check: the PC this copy runs on restarts (or loses
    /// power): TailCat is gone first, so nothing more reaches anyone, not
    /// even a goodbye, and the app with it.
    #[cfg(any(all(test, target_os = "linux"), feature = "e2e"))]
    pub fn test_power_off(&mut self) {
        if let Some(p) = self.proc.as_mut() {
            p.kill();
        }
        self.close(false);
    }

    /// For the end-to-end check: events from two hubs in one process are
    /// told apart by generation.
    #[cfg(any(all(test, target_os = "linux"), feature = "e2e"))]
    pub fn test_gen_base(&mut self, base: u64) {
        self.gen = base;
    }

    pub fn json(&self) -> Value {
        let now = hubstore::now();
        json!({
            "st": self.st,
            "mode": match self.mode { Mode::Off => "off", Mode::Hosting => "host", Mode::Joined => "join" },
            "error": self.error,
            "notice": self.notice,
            "name": self.name,
            "id": self.id,
            "kind": if self.expires.is_some() { "saved" } else { "temporary" },
            "expiresIn": self.expires.map(|e| e.saturating_sub(now)),
            "savedHere": self.saved_id.is_some(),
            "canSave": self.mode == Mode::Joined && self.is_live() && self.expires.is_some() && self.saved_id.is_none(),
            "making": self.making,
            "code": self.code,
            "me": self.me,
            "members": self.members,
            "invites": self.invites.iter().map(|i| json!({
                "id": i.id, "from": i.from, "kind": i.kind, "code": i.code, "title": i.title,
                "ago": i.at.elapsed().as_secs(),
            })).collect::<Vec<_>>(),
            "offers": self.offers,
            "openToHub": self.open_to_hub,
            "sent": self.sent,
            "reconnecting": self.reconnecting,
            "link": self.link.json(),
            "log": self.log.json(),
            "saved": self.saved.iter().map(|s| {
                let scan = self.scans.get(&s.id).filter(|sc| !sc.quiet);
                json!({
                    "id": s.id, "name": s.name, "expiresIn": s.expires.saturating_sub(now),
                    "people": s.book.len(),
                    // When its address book last changed (another copy's had someone new).
                    "updatedAgo": (s.updated > 0).then(|| now.saturating_sub(s.updated)),
                    "here": self.is_on() && self.id == s.id,
                    "scan": scan.map(|sc| json!({
                        "running": sc.checked < sc.total, "checked": sc.checked, "total": sc.total, "learned": sc.learned,
                        // Open is checking whether anyone has it open first.
                        "opening": sc.open_after && sc.checked < sc.total,
                        "hits": sc.hits.iter().map(|h| json!({ "code": h.code, "host": h.host, "members": h.members })).collect::<Vec<_>>(),
                    })),
                })
            }).collect::<Vec<_>>(),
            "prompt": self.prompt.as_ref().map(|(id, hits)| json!({
                "id": id,
                "name": self.saved.iter().find(|s| s.id == *id).map(|s| s.name.clone()).unwrap_or_default(),
                "hits": hits.iter().map(|h| json!({ "code": h.code, "host": h.host, "members": h.members })).collect::<Vec<_>>(),
            })),
        })
    }
}

/// "a voice room", "a file share"… for messages.
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        "voip" => "a voice room",
        "file" => "a file share",
        "game" => "a game server",
        "mount" => "a shared folder",
        _ => "some ports",
    }
}
