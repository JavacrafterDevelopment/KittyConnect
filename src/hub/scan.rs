//! Scan and Open for saved hubs: who has a copy open, and joining up
//! when two copies are open at once.

use super::*;

/// Scan checks this many codes at once (each is a TailCat of its own for a moment).
pub(super) const SCAN_AT_ONCE: usize = 8;
/// How long Scan waits for each copy to answer.
pub(super) const SCAN_WAIT: Duration = Duration::from_secs(20);
/// While we host a saved hub, how long until we look again for another copy
/// of it that's open too (someone took over while we were gone, or opened
/// theirs not knowing where it was): soon at first, then every minute.
pub(super) fn look_after(n: u32) -> Duration {
    Duration::from_secs(match n {
        0 => 10,
        1 => 20,
        2 => 40,
        _ => 60,
    })
}
/// When another open copy looks at ours, we look back, but no more often
/// than this.
pub(super) const LOOK_BACK_EVERY: Duration = Duration::from_secs(30);

/// Two copies of a saved hub are open at once, ours (at `mine`, `ours`
/// people in it) and another (at `code`, `theirs`): does everyone belong in
/// theirs? The one with more people in it wins; with the same, the lower
/// code. Both sides ask it the same way round, so exactly one moves.
pub(super) fn theirs_wins(mine: &str, ours: u64, code: &str, theirs: u64) -> bool {
    theirs > ours || (theirs == ours && code < mine)
}

/// A scan of a saved hub's address book: who has it open right now.
#[derive(Default)]
pub(super) struct Scan {
    pub(super) total: usize,
    pub(super) checked: usize,
    pub(super) hits: Vec<Hit>,
    /// Every code looked at (people an open copy told us about get looked at too).
    pub(super) seen: Vec<String>,
    /// People new to us, from open copies' books.
    pub(super) learned: usize,
    /// Started by Open: when it's done, open ours, or ask.
    pub(super) open_after: bool,
    /// Our own look while we host it, for another open copy to join up with
    /// (not shown).
    pub(super) quiet: bool,
    /// Which scan this is: answers to an older one are dropped.
    pub(super) no: u64,
    /// Ends the checks still going, once they're no use.
    pub(super) stop: Arc<Stop>,
}

/// Ends a scan's checks early (we joined or opened a hub, or Cancel): none
/// more start, and the TailCat of each one still going is closed, which ends
/// its wait at once.
#[derive(Default)]
pub(super) struct Stop {
    pub(super) stopped: AtomicBool,
    pub(super) running: Mutex<Vec<Arc<Mutex<Option<Proc>>>>>,
}

impl Stop {
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        for p in self.running.lock().unwrap().drain(..) {
            p.lock().unwrap().take();
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Keeps a check's TailCat where `stop` can close it; None (and it's
    /// closed) if the scan has been stopped already.
    pub(super) fn hold(&self, p: Proc) -> Option<Arc<Mutex<Option<Proc>>>> {
        let mut running = self.running.lock().unwrap();
        if self.is_stopped() {
            return None;
        }
        let slot = Arc::new(Mutex::new(Some(p)));
        running.push(slot.clone());
        Some(slot)
    }
}

/// Someone's copy of a saved hub, open right now.
#[derive(Clone, Debug)]
pub struct Hit {
    pub code: String,
    /// Whoever has it open, by their current name.
    pub host: String,
    pub members: u64,
}

/// What the app should do once a scan for Open has finished.
pub enum AfterScan {
    /// Nobody else has it open: open ours.
    OpenOwn(String),
    /// Something to say (we moved to another open copy).
    Said(String, &'static str),
}

impl Hub {
    /// Looks through a saved hub's address book for whoever has it open now
    /// (everyone's code but ours). `open_after`: then open ours if nobody
    /// does, or ask.
    pub fn scan(&mut self, id: &str, open_after: bool, proxy: &EventLoopProxy<UserEvent>) -> Option<AfterScan> {
        let s = self.saved.iter().find(|s| s.id == id)?;
        let targets: Vec<Entry> = s.book.iter().filter(|e| e.code != s.code).cloned().collect();
        // One of ours is still going (a quiet look of our own gives way).
        if self.scans.get(id).is_some_and(|sc| sc.checked < sc.total && !sc.quiet) {
            return None;
        }
        self.prompt = None;
        let empty = targets.is_empty();
        self.start_scan(id, targets, open_after, false, proxy);
        (empty && open_after).then(|| AfterScan::OpenOwn(id.to_string()))
    }

    pub(super) fn start_scan(&mut self, id: &str, targets: Vec<Entry>, open_after: bool, quiet: bool, proxy: &EventLoopProxy<UserEvent>) {
        // One still going for this hub (a quiet look of ours) gives way.
        self.stop_scans(Some(id));
        self.scan_no += 1;
        let no = self.scan_no;
        let seen = targets.iter().map(|e| e.code.clone()).collect();
        let stop = Arc::new(Stop::default());
        self.scans.insert(id.to_string(), Scan { total: targets.len(), open_after, quiet, no, seen, stop: stop.clone(), ..Scan::default() });
        // Our own looks while we host it say so, so a copy we find looks back.
        let mine = quiet.then(|| self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone())).flatten();
        if quiet {
            self.last_look = Some(Instant::now());
        }
        if !targets.is_empty() {
            look(targets, id.to_string(), no, self.tag, mine, stop, proxy);
        }
    }

    /// Ends the checks still going (of hub `only`, or every hub's): we've
    /// joined or opened a hub, or Cancel was pressed. Their TailCats close,
    /// and answers still on the way are dropped; what was found stays.
    pub(super) fn stop_scans(&mut self, only: Option<&str>) {
        for (_, sc) in self.scans.iter_mut().filter(|(id, _)| only.is_none_or(|o| o == id.as_str())) {
            if sc.checked < sc.total {
                sc.stop.stop();
                sc.total = sc.checked;
                sc.open_after = false;
                // No scan has number 0: whatever else comes for this one is dropped.
                sc.no = 0;
            }
        }
    }

    /// Another copy of the saved hub we host looked at ours: look at it
    /// (once in a while at most, so two copies don't keep looking at each
    /// other), and if it's where everyone should be, go there.
    pub(super) fn look_back(&mut self, code: &str) {
        if self.mode != Mode::Hosting || !self.is_live() || self.expires.is_none() {
            return;
        }
        let id = self.id.clone();
        let busy = self.scans.get(&id).is_some_and(|sc| sc.checked < sc.total);
        if busy || self.last_look.is_some_and(|t| t.elapsed() < LOOK_BACK_EVERY) {
            // Not right now: our next look at everyone comes early instead.
            let soon = self.last_look.map_or_else(Instant::now, |t| t + LOOK_BACK_EVERY).max(Instant::now() + Duration::from_secs(5));
            if let Some((at, _)) = self.watch.as_mut().filter(|(at, _)| *at > soon) {
                *at = soon;
            }
            return;
        }
        let Some(proxy) = self.proxy.clone() else { return };
        let Some(s) = self.saved.iter().find(|s| s.id == id) else { return };
        if code == s.code {
            return;
        }
        // Whose it is, if they're in our book; the look finds out anyway.
        let name = s.book.iter().find(|e| e.code == code).map(|e| e.name.clone()).unwrap_or_else(|| "Someone".into());
        let target = Entry { name, code: code.to_string(), at: 0 };
        self.start_scan(&id, vec![target], false, true, &proxy);
    }

    /// While we host a saved hub: is another copy of it open too? Whoever is
    /// in here with us isn't hosting it anywhere else, so only the others
    /// are looked at.
    pub(super) fn look_for_others(&mut self) {
        let Some(proxy) = self.proxy.clone() else { return };
        let id = self.id.clone();
        let Some(s) = self.saved.iter().find(|s| s.id == id) else { return };
        let here: Vec<&str> = self.members.iter().filter_map(|m| m["code"].as_str()).collect();
        let targets: Vec<Entry> = s.book.iter().filter(|e| e.code != s.code && !here.contains(&e.code.as_str())).cloned().collect();
        if targets.is_empty() || self.scans.get(&id).is_some_and(|sc| sc.checked < sc.total) {
            return;
        }
        self.start_scan(&id, targets, false, true, &proxy);
    }

    /// Two copies of our saved hub are open at once (ours, and `host`'s at
    /// `code` with `theirs` people in it): everyone ends up in one. The one
    /// with more people in it stays (the same: the lower code); if that's
    /// theirs, everyone here goes there, us too. Both sides work it out the
    /// same way, so only one moves.
    pub(super) fn join_up(&mut self, id: &str, code: &str, host: &str, theirs: u64) -> Option<AfterScan> {
        if self.mode != Mode::Hosting || self.id != id || !self.is_live() {
            return None;
        }
        let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone())?;
        let ours = self.members.len() as u64;
        if code == mine || theirs == 0 || !theirs_wins(&mine, ours, code, theirs) {
            return None;
        }
        let (Some(proxy), me) = (self.proxy.clone(), self.me_name.clone()) else { return None };
        let (hub, expires, others) = (self.name.clone(), self.expires, ours.saturating_sub(1));
        // Everyone here hears where to go first; our copy stays up a moment
        // so they do.
        if let Some(server) = &self.server {
            server.send_moving(code, host);
        }
        let (server, proc) = (self.server.take(), self.proc.take());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            drop(server);
            drop(proc);
        });
        self.plan_failover();
        self.join(code, &me, &proxy);
        if self.mode == Mode::Joined {
            self.id = id.to_string();
            self.expires = expires;
        }
        let people = if theirs == 1 { "person" } else { "people" };
        let with = match others {
            0 => String::new(),
            1 => ", and so did the one person in yours".into(),
            n => format!(", and so did the {n} people in yours"),
        };
        Some(AfterScan::Said(format!("{host} had {hub} open too ({theirs} {people}): you moved there{with}."), "ok"))
    }

    /// One code's answer to a scan. An open copy's book is merged into ours,
    /// and anyone new in it is looked at too.
    pub fn on_scan(&mut self, id: &str, no: u64, entry: Entry, result: Result<Peeked, String>, proxy: &EventLoopProxy<UserEvent>) -> Option<AfterScan> {
        // An answer to a scan that's been replaced since.
        if self.scans.get(id).is_none_or(|sc| sc.no != no) {
            return None;
        }
        let found = result.ok().filter(|p| p.id == id);
        if let Some(p) = &found {
            let learned = self.learn(id, &p.book, &p.gone).unwrap_or(0);
            let mine = self.saved.iter().find(|s| s.id == id).map(|s| s.code.clone()).unwrap_or_default();
            let book: Vec<Entry> = self.saved.iter().find(|s| s.id == id).map(|s| s.book.clone()).unwrap_or_default();
            let sc = self.scans.get_mut(id)?;
            sc.learned += learned;
            let more: Vec<Entry> = book.into_iter().filter(|e| e.code != mine && !sc.seen.contains(&e.code)).collect();
            if !more.is_empty() {
                sc.seen.extend(more.iter().map(|e| e.code.clone()));
                sc.total += more.len();
                let from = sc.quiet.then_some(mine);
                look(more, id.to_string(), no, self.tag, from, sc.stop.clone(), proxy);
            }
        }
        let sc = self.scans.get_mut(id)?;
        sc.checked += 1;
        // Only a copy of this same hub counts. Each one shows the moment
        // it's found, while the rest are still being looked at.
        if let Some(p) = found {
            let host = if p.host.is_empty() { entry.name.clone() } else { p.host };
            sc.hits.retain(|h| h.code != entry.code);
            sc.hits.push(Hit { code: entry.code.clone(), host: host.clone(), members: p.members });
            if sc.quiet {
                return self.join_up(id, &entry.code, &host, p.members);
            }
            // Open: it's already online, so offer to join at once (anyone
            // else found joins the list).
            if sc.open_after {
                self.prompt = Some((id.to_string(), sc.hits.clone()));
            }
        }
        if sc.checked < sc.total || !sc.open_after {
            return None;
        }
        // Open, and every look is done: nobody has it open, so ours opens.
        sc.open_after = false;
        sc.hits.is_empty().then(|| AfterScan::OpenOwn(id.to_string()))
    }

    /// Merges another copy's book into our saved copy of hub `id`, newest
    /// wins, and keeps it. Returns how many people are new to us (None if we
    /// don't have it saved).
    pub(super) fn learn(&mut self, id: &str, book: &[Entry], gone: &[Gone]) -> Option<usize> {
        let s = self.saved.iter_mut().find(|s| s.id == id)?;
        let before: Vec<String> = s.book.iter().map(|e| e.code.clone()).collect();
        if !hubstore::merge(&mut s.book, &mut s.gone, book, gone) {
            return Some(0);
        }
        s.updated = hubstore::now();
        let _ = self.store.store(s);
        Some(s.book.iter().filter(|e| !before.contains(&e.code) && e.code != s.code).count())
    }

}

/// Looks at each code (a few at a time) and reports each answer to the app
/// the moment it comes. `mine`: our code, when we have this hub open
/// ourselves. `stop` ends it early.
pub(super) fn look(targets: Vec<Entry>, id: String, no: u64, tag: u64, mine: Option<String>, stop: Arc<Stop>, proxy: &EventLoopProxy<UserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        for chunk in targets.chunks(SCAN_AT_ONCE) {
            if stop.is_stopped() {
                return;
            }
            let looks: Vec<_> = chunk
                .iter()
                .cloned()
                .map(|e| {
                    let (p, id, mine, stop) = (proxy.clone(), id.clone(), mine.clone(), stop.clone());
                    std::thread::spawn(move || {
                        let r = scan_one(&e, mine.as_deref(), &stop, &p);
                        let _ = p.send_event(UserEvent::HubScan(tag, id, no, e, r));
                    })
                })
                .collect();
            for l in looks {
                let _ = l.join();
            }
        }
    });
}

/// One look at someone's copy: a TailCat to their code for a moment, and a peek.
pub(super) fn scan_one(e: &Entry, mine: Option<&str>, stop: &Stop, proxy: &EventLoopProxy<UserEvent>) -> Result<Peeked, String> {
    if stop.is_stopped() {
        return Err("stopped".into());
    }
    let c = code::parse(&e.code)?;
    let local = crate::free_port()?;
    let args = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
    let Some(slot) = stop.hold(tailcat::spawn(Role::HubScan, &args, proxy)?) else { return Err("stopped".into()) };
    let r = peek_from(local, SCAN_WAIT, mine, &stop.stopped);
    slot.lock().unwrap().take();
    r
}
