//! What the App does with events from TailCat, shares, the tick,
//! LAN games and pings, and retrying what dropped.

use crate::*;

impl App {
    pub(crate) fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
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
                            // Joiners can now ask whether the port they typed is one of these.
                            peers::serve(&addr, &h.ports);
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
                        tc_line(&mut j.log, &mut j.link, line);
                        // TailCat found a path to the host: it's there.
                        if j.promote() {
                            self.status = ("Connected to your friend. Use the address shown.".into(), "ok");
                        }
                    }
                    TcEvent::Exited => {
                        let why = last_words(&j.log);
                        j.proc = None;
                        j.relays.clear();
                        j.st = St::Error;
                        j.error = format!("TailCat stopped. {why}");
                    }
                }
            }
            Role::VoipHost | Role::VoipJoin => return self.voip.on_tailcat(role, id, ev),
            Role::HubHost | Role::HubJoin => return self.hub.on_tailcat(role, id, ev),
            Role::GameHost | Role::GameJoin => return self.api.on_tailcat(role, id, ev),
            _ => return false,
        }
        true
    }

    pub(crate) fn on_share(&mut self, ev: ShareEvent) -> bool {
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
                    self.share.log.push(format!("Portside: sent {name}"));
                } else {
                    self.share.log.push(format!("Portside: transfer of {name} was interrupted"));
                }
            }
        }
        true
    }

    pub(crate) fn on_recv(&mut self, ev: RecvEvent) -> bool {
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

    pub(crate) fn on_tick(&mut self) -> bool {
        let mut changed = false;
        // A joined port is "Connected" once the host has answered; until then
        // it's reaching them, and after a while says it can't (and keeps trying).
        let mut reached = false;
        for j in &mut self.joins {
            reached |= j.promote();
            if j.unreachable() != j.told_unreachable {
                j.told_unreachable = j.unreachable();
                changed = true;
            }
        }
        if reached {
            self.status = ("Connected to your friend. Use the address shown.".into(), "ok");
            changed = true;
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
            let r = Report {
                peer: self.recv.peer,
                addr: &self.recv.addr,
                path: self.recv.link.path(),
                rtt: self.recv.link.rtt_ms,
                ports: Vec::new(),
            };
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
        self.mounts_tick(now);
        changed |= self.voip.tick();
        if let Some((text, kind)) = self.hub.tick() {
            self.set_status(text, kind);
            changed = true;
        }
        // "Open to hub": what's open goes to the hub (and stops once it closes).
        let open = self.invitables().into_iter().map(|i| (i.what, i.title, i.code)).collect();
        self.hub.sync_offers(open);
        // The game API: sessions and activity to the hub, and anyone asking too long.
        changed |= self.api_tick();
        changed
    }

    pub(crate) fn open_saved_hub(&mut self, id: &str) {
        self.hub.open_saved(id, &self.name, &self.proxy);
        if self.hub.error.is_empty() {
            self.set_status("Opening the hub…", "busy");
        }
    }

    pub(crate) fn on_hub_key(&mut self, saved: hubstore::Saved, result: Result<String, String>, open_after: bool) {
        if let Some((text, kind)) = self.hub.on_key(saved, result, open_after, &self.name, &self.proxy) {
            self.set_status(text, kind);
        } else if !self.hub.error.is_empty() {
            let e = self.hub.error.clone();
            self.set_status(e, "bad");
        }
    }

    pub(crate) fn on_hub_scan(&mut self, id: &str, no: u64, entry: hubstore::Entry, result: Result<hub::Peeked, String>) {
        match self.hub.on_scan(id, no, entry, result, &self.proxy) {
            Some(hub::AfterScan::OpenOwn(id)) => self.open_saved_hub(&id),
            Some(hub::AfterScan::Said(text, kind)) => self.set_status(text, kind),
            None => {}
        }
    }

    /// Minecraft: keeps hosts' notes up to date, asks hosts we joined which
    /// LAN worlds they have open, and shows those in this PC's Minecraft.
    pub(crate) fn games_tick(&mut self, now: Instant) -> bool {
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
    pub(crate) fn probe(&mut self, now: Instant) {
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

    pub(crate) fn on_probed(&mut self, found: Vec<(u16, bool)>) -> bool {
        self.probing = false;
        self.probe_at = Instant::now() + Duration::from_secs(2);
        self.listening.extend(found);
        let mut changed = false;
        // A manual port warns until something is running on every one of its ports.
        for h in self.hosts.iter_mut().filter(|h| h.proc.is_some() && h.game.is_none() && h.mount.is_none()) {
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

    pub(crate) fn on_lan(&mut self, key: u64, result: Result<Vec<lan::World>, String>) -> bool {
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

    pub(crate) fn spawn_ping(&self, target: PingTarget, port: u16, request: String) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = tailcat::ping_port(port, &request, Duration::from_secs(10));
            let _ = proxy.send_event(UserEvent::Ping(target, result));
        });
    }

    pub(crate) fn on_ping(&mut self, target: PingTarget, result: Result<tailcat::Pong, String>) -> bool {
        let name = result.as_ref().ok().and_then(|p| p.name.clone());
        let retry = result.as_ref().is_ok_and(|p| p.retry);
        let ports = result.as_ref().ok().map(|p| p.ports);
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
                    self.recv.log.push("Portside: the sharer asked to retry a direct connection");
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
                match j.checked(result, ports.flatten()) {
                    Checked::WrongPorts => {
                        self.status = (j.error.clone(), "bad");
                        return true;
                    }
                    Checked::Gone => self.status = ("A joined port was disconnected by the other side.".into(), "bad"),
                    Checked::Connected => self.status = ("Connected to your friend. Use the address shown.".into(), "ok"),
                    Checked::Nothing => {}
                }
                if retry && j.proc.is_some() && j.link.path() == "relay" {
                    j.log.push("Portside: the host asked to retry a direct connection");
                    self.retry_join(key);
                }
            }
        }
        true
    }

    /// Restarts our end of a file-share tunnel to try for a direct path from
    /// scratch. The file list stays; a download in progress fails and can be
    /// started again.
    pub(crate) fn retry_recv(&mut self) {
        if self.recv.st != St::Live || self.recv.args.is_empty() {
            return;
        }
        self.recv.proc = None; // ends the old tunnel
        match tailcat::spawn(Role::RecvForward, &self.recv.args, &self.proxy) {
            Ok(p) => {
                self.recv.proc = Some(p);
                self.recv.link.restarted();
                self.recv.log.push("Portside: reconnecting to try for a direct connection");
                self.set_status("Reconnecting to try for a direct connection…", "busy");
            }
            Err(e) => self.fail_recv(e),
        }
    }

    /// The same for a joined port set. Anything connected through it (a game)
    /// drops for a moment and reconnects to the same local port.
    pub(crate) fn retry_join(&mut self, key: u64) {
        let Some(j) = self.joins.iter_mut().find(|j| j.key == key && j.proc.is_some()) else { return };
        j.proc = None;
        match tailcat::spawn(Role::PortJoin, &j.args, &self.proxy) {
            Ok(p) => {
                j.proc = Some(p);
                j.st = St::Starting;
                j.started = Instant::now();
                j.ports_checked = false;
                j.link.restarted();
                j.log.push("Portside: reconnecting to try for a direct connection");
                self.status = ("Reconnecting to try for a direct connection…".into(), "busy");
            }
            Err(e) => {
                j.relays.clear();
                j.st = St::Error;
                j.error = e;
            }
        }
    }
}
