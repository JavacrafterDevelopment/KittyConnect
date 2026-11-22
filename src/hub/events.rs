//! What the Hub does with what happens: hub messages, TailCat, and the
//! once-a-second tick (pings, take-overs, failover).

use super::*;

impl Hub {
    pub fn on_tailcat(&mut self, role: Role, id: u64, ev: TcEvent) -> bool {
        if self.proc.as_ref().map(|p| p.id) != Some(id) || !matches!(role, Role::HubHost | Role::HubJoin) {
            return false;
        }
        match ev {
            TcEvent::Line(line) => {
                if role == Role::HubHost && !tailcat::is_verbose(&line) {
                    if let (Some(addr), Some(srv)) = (tailcat::find_addr(&line), &self.server) {
                        self.code = Some(code::hub(srv.port, &addr));
                        // Our own connection to it is already in (it's on this PC).
                        if self.me != 0 {
                            self.st = "live";
                        }
                    }
                }
                self.link.observe(&line);
                if !tailcat::is_verbose(&line) || tailcat::keep_verbose(&line) {
                    self.log.push(line);
                }
            }
            TcEvent::Exited => {
                let why = crate::last_words(&self.log);
                self.end(&format!("TailCat stopped. {why}"));
            }
        }
        true
    }

    /// Takes in what the hub said. Returns a line for the status bar, if any.
    pub fn on_event(&mut self, gen: u64, ev: HubEvent) -> Status {
        if gen != self.gen {
            return None;
        }
        match ev {
            HubEvent::Welcome { you, hub, id, expires_in, host } => {
                self.me = you;
                if !hub.is_empty() {
                    self.name = hub;
                }
                if self.mode == Mode::Joined {
                    self.id = id;
                    self.expires = expires_in.map(|s| hubstore::now() + s);
                    // A hub we saved before: our code goes in its book again.
                    if self.expires.is_some() && self.saved.iter().any(|s| s.id == self.id) {
                        self.saved_id = Some(self.id.clone());
                    }
                }
                self.share_address();
                // A reconnect starts the hub's list of what we have open afresh.
                self.offered.clear();
                // Our own hub is live once TailCat has given it a code.
                let was = self.st;
                self.reconnecting = false;
                // The hub answered through the tunnel: it's up, direct or relay.
                if self.mode == Mode::Joined {
                    self.link.peer_seen = true;
                }
                if self.mode == Mode::Joined || self.code.is_some() {
                    self.st = "live";
                }
                self.lost_since = None;
                self.failover = None;
                // Hosting a saved hub: someone may have it open elsewhere
                // too (they took over while we were gone); look soon.
                if self.mode == Mode::Hosting && self.expires.is_some() {
                    self.watch = Some((Instant::now() + look_after(0), 0));
                }
                // A saved hub we joined is saved here too, by itself: our own
                // key and code for it, so we can open it ourselves.
                let auto_save = self.mode == Mode::Joined && self.expires.is_some() && self.saved_id.is_none() && self.making.is_none();
                if auto_save {
                    if let Some(p) = self.proxy.clone() {
                        self.save_joined(&p);
                    }
                }
                if was != "live" && self.st == "live" {
                    let msg = match (host, auto_save) {
                        (true, _) => format!("{} is open. Send its code to your friends.", self.name),
                        (false, true) => format!("Joined {}, and saved it here too.", self.name),
                        (false, false) => format!("Joined {}.", self.name),
                    };
                    return Some((msg, "ok"));
                }
            }
            HubEvent::Members(list) => {
                // Everyone else left our saved hub (or vanished): they may
                // be in another copy of it now, so look again soon.
                if self.mode == Mode::Hosting && self.expires.is_some() && list.len() <= 1 && self.members.len() > 1 {
                    self.watch = Some((Instant::now() + look_after(0), 0));
                }
                self.members = list;
            }
            HubEvent::Book { book, gone } => {
                self.book = book;
                self.gone = gone;
                // Our saved copy catches up: whoever came back after a while
                // gets everyone added since, and nothing older wins.
                let new = self.saved_id.clone().and_then(|id| self.learn(&id, &self.book.clone(), &self.gone.clone()));
                // Our "delete for me" reached the hub: finish it.
                if let Some(f) = self.finish.as_ref().filter(|f| !f.everyone) {
                    let id = f.id.clone();
                    let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
                    if mine.is_some_and(|c| self.gone.iter().any(|g| g.code == c)) {
                        return self.finish_delete(&id, None);
                    }
                }
                if let Some(n) = new.filter(|n| *n > 0) {
                    return Some((format!("{}'s address book: {n} new {}.", self.name, if n == 1 { "person" } else { "people" }), "ok"));
                }
            }
            HubEvent::Offers(list) => {
                // Checked again here: only codes we can join, from others.
                self.offers = list
                    .into_iter()
                    .filter(|o| o["from"].as_u64() != Some(self.me))
                    .filter_map(|o| {
                        let (kind, code) = invitation(o["code"].as_str()?)?;
                        Some(json!({
                            "from": o["from"], "oid": clean(o["oid"].as_str()?, 64), "kind": kind, "code": code,
                            "name": clean(o["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME),
                            "title": clean(o["title"].as_str().unwrap_or(""), MAX_TITLE),
                        }))
                    })
                    .take(MAX_MEMBERS * MAX_OFFERS_EACH)
                    .collect();
            }
            HubEvent::Deleted { by } => {
                let id = self.id.clone();
                return self.finish_delete(&id, Some(by));
            }
            HubEvent::Rtt(ms) => self.link.rtt_ms = Some(ms),
            HubEvent::Invite(v) => {
                // Checked again here: whatever comes in, only codes we can
                // join, written out clean.
                let Some((kind, code_text)) = invitation(v["code"].as_str().unwrap_or_default()) else { return None };
                if v["from"].as_u64() == Some(self.me) {
                    return None;
                }
                let from = clean(v["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME);
                let title = clean(v["title"].as_str().unwrap_or(""), MAX_TITLE);
                // The same invite again (they pressed twice) replaces the old one.
                self.invites.retain(|i| i.code != code_text);
                let id = self.next_invite;
                self.next_invite += 1;
                self.invites.insert(0, Invite { id, from: from.clone(), kind, code: code_text, title, at: Instant::now() });
                self.invites.truncate(MAX_INVITES);
                return Some((format!("{from} invited you to {}. Open the Hub tab to join.", kind_name(kind)), "ok"));
            }
            HubEvent::Invited { n, error } => {
                self.sent = if !error.is_empty() {
                    error
                } else if n == 0 {
                    "Nobody else is in the hub to invite.".into()
                } else {
                    format!("Invited {n} {}.", if n == 1 { "person" } else { "people" })
                };
            }
            HubEvent::Reconnecting => {
                if self.st == "live" {
                    self.reconnecting = true;
                }
                // A saved hub's host is gone (or the one we were taking over
                // to never came): someone else takes over before long.
                let taking_over = self.failover.as_ref().is_some_and(|(id, _)| *id == self.id);
                if self.mode == Mode::Joined && self.expires.is_some() && (self.st == "live" || taking_over) && self.lost_since.is_none() {
                    self.lost_since = Some(Instant::now());
                }
            }
            HubEvent::Moving { code, name } => return self.move_to(&code, &name),
            HubEvent::AlsoOpen { code } => self.look_back(&code),
            HubEvent::Kicked => {
                self.end("The host removed you from the hub.");
                return Some(("Removed from the hub.".into(), "bad"));
            }
            HubEvent::Full => self.end("That hub is full."),
            HubEvent::Old => self.end("That hub runs a different version of Portside. Update both to the latest."),
        }
        None
    }

    /// The saved hub moves to `code` (`name`'s copy): ours, so we open it
    /// (everyone comes to us), or theirs, so we go there.
    pub(super) fn move_to(&mut self, code: &str, name: &str) -> Status {
        let (Some(proxy), me) = (self.proxy.clone(), self.me_name.clone()) else { return None };
        let (id, hub, expires) = (self.id.clone(), self.name.clone(), self.expires);
        // Who's next if that copy never comes (worked out now, while we still
        // have the member list).
        self.plan_failover();
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone());
        if mine.as_deref() == Some(code) {
            self.open_saved(&id, &me, &proxy);
            return Some((format!("You're hosting {hub} now; everyone is coming over."), "ok"));
        }
        self.join(code, &me, &proxy);
        // Still this hub until that copy answers, so if it never does, we go
        // to the next in line rather than wait for it forever.
        if self.mode == Mode::Joined {
            self.id = id;
            self.expires = expires;
        }
        Some((format!("Moving to {name}'s copy of {hub}…"), "busy"))
    }

    /// Everyone's app works out the same list of who takes over a saved hub
    /// (those in it who saved it, longest in first), from the last member
    /// list, once per hub.
    pub(super) fn plan_failover(&mut self) {
        if self.failover.as_ref().is_some_and(|(f, _)| *f == self.id) {
            return;
        }
        let mut members: Vec<&Value> = self.members.iter().filter(|m| m["host"] != true).collect();
        members.sort_by_key(|m| m["id"].as_u64().unwrap_or(u64::MAX));
        let list = members
            .into_iter()
            .filter_map(|m| hubstore::entry(m["name"].as_str().unwrap_or(""), m["code"].as_str()?, 0))
            .collect();
        self.failover = Some((self.id.clone(), list));
    }

    /// A saved hub's host vanished: everyone's app takes the same list of who
    /// takes over (those in it who saved it, longest in first) and goes to the
    /// first one; if that one never comes, the next.
    pub(super) fn take_over(&mut self) -> Status {
        self.plan_failover();
        let lost = self.code.clone().unwrap_or_default();
        let next = self.failover.as_mut().and_then(|(_, l)| {
            while !l.is_empty() {
                let e = l.remove(0);
                if e.code != lost {
                    return Some(e);
                }
            }
            None
        });
        self.lost_since = None;
        match next {
            Some(e) => self.move_to(&e.code, &e.name),
            None => {
                self.failover = None;
                let hub = self.name.clone();
                self.end(&format!("{hub}'s host is gone, and nobody else in it can take over. Open it yourself, or Scan."));
                Some((self.notice.clone(), "bad"))
            }
        }
    }

    /// Once a second: our path and ping to the hub, a delete that's waited
    /// long enough, saved hubs that have expired, and taking over a saved
    /// hub whose host is gone.
    pub fn tick(&mut self) -> Status {
        // The host vanished; or the copy we went to after that never came.
        let limit = if self.failover.as_ref().is_some_and(|(f, _)| *f == self.id) { NEXT_AFTER } else { TAKE_OVER_AFTER };
        if self.lost_since.is_some_and(|t| t.elapsed() >= limit) && self.mode == Mode::Joined {
            return self.take_over();
        }
        if let Some((_, n)) = self.watch.filter(|(at, _)| Instant::now() >= *at) {
            self.watch = Some((Instant::now() + look_after(n + 1), n + 1));
            if self.mode == Mode::Hosting && self.is_live() && self.expires.is_some() {
                self.look_for_others();
            }
        }
        if let Some((id, _)) = self.rescan.as_ref().filter(|(_, at)| Instant::now() >= *at) {
            let id = id.clone();
            self.rescan = None;
            if let (Some(p), false) = (self.proxy.clone(), self.is_on()) {
                self.scan(&id, false, &p);
            }
        }
        if let Some(f) = self.finish.as_ref().filter(|f| Instant::now() >= f.until) {
            let id = f.id.clone();
            return self.finish_delete(&id, None);
        }
        let now = hubstore::now();
        if let Some(s) = self.saved.iter().find(|s| s.expires <= now) {
            let (id, name) = (s.id.clone(), s.name.clone());
            self.store.delete(&id);
            self.saved.retain(|s| s.id != id);
            self.scans.remove(&id);
            if self.is_on() && self.id == id {
                self.close(false);
            }
            self.notice = format!("{name} expired and was deleted.");
            return Some((self.notice.clone(), "idle"));
        }
        if self.mode == Mode::Joined && self.is_live() {
            let (path, rtt) = (self.link.path(), self.link.rtt_ms);
            let moved = match (self.reported_rtt, rtt) {
                (None, Some(_)) => true,
                (Some(a), Some(b)) => a.abs_diff(b) >= 5.max(a / 7),
                _ => false,
            };
            if path != self.reported || moved {
                self.reported = path;
                self.reported_rtt = rtt;
                if let Some(c) = &self.conn {
                    c.send(json!({ "op": "path", "path": path, "rtt": rtt }));
                }
            }
        }
        None
    }

}
