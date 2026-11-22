//! The hub's server, on the host's PC: members, offers and invites.

use super::*;

pub(super) struct Member {
    pub(super) id: u64,
    /// A random number the member picked, so a reconnect replaces its old
    /// entry instead of listing it twice. Never shown to anyone.
    pub(super) peer: u64,
    pub(super) name: String,
    pub(super) host: bool,
    pub(super) path: &'static str,
    pub(super) rtt: Option<u32>,
    pub(super) out: Sender<Value>,
    pub(super) stream: TcpStream,
    pub(super) invites: VecDeque<Instant>,
    /// Their own code for a saved hub, once they've said (a rename updates it).
    pub(super) code: Option<String>,
}

/// What a hub is, the same in everyone's copy of a saved one.
#[derive(Clone, Debug)]
pub struct Meta {
    /// 32 hex digits.
    pub id: String,
    pub name: String,
    /// When a saved hub is deleted (seconds since 1970); None for a temporary one.
    pub expires: Option<u64>,
}

/// Something one person has open, shown to everyone ("Open to hub").
pub(super) struct Offer {
    pub(super) from: u64,
    pub(super) oid: String,
    pub(super) kind: &'static str,
    pub(super) code: String,
    pub(super) title: String,
}

pub(super) struct State {
    pub(super) meta: Meta,
    pub(super) members: Vec<Member>,
    pub(super) next_id: u64,
    pub(super) next_inv: u64,
    pub(super) book: Vec<Entry>,
    pub(super) gone: Vec<Gone>,
    pub(super) offers: Vec<Offer>,
}

impl State {
    pub(super) fn expires_in(&self) -> Option<u64> {
        self.meta.expires.map(|e| e.saturating_sub(hubstore::now()))
    }

    pub(super) fn broadcast(&self, msg: &Value) {
        for m in &self.members {
            let _ = m.out.send(msg.clone());
        }
    }

    /// The time for a change made here to `code`: now, or just after the
    /// last change the book has for it, so it always counts as newer (two
    /// changes in the same second are still in order).
    pub(super) fn stamp(&self, code: &str) -> u64 {
        let last = self.book.iter().filter(|e| e.code == code).map(|e| e.at).chain(self.gone.iter().filter(|g| g.code == code).map(|g| g.at)).max();
        last.map_or(hubstore::now(), |l| hubstore::now().max(l + 1))
    }

    /// The book, with deleted codes both ways: plain (what 0.12.0 reads) and
    /// with their times.
    pub(super) fn book_msg(&self) -> Value {
        json!({ "op": "book", "book": self.book, "gone": hubstore::gone_codes(&self.gone), "goneAt": self.gone })
    }

    pub(super) fn offers_msg(&self) -> Value {
        let list: Vec<Value> = self
            .offers
            .iter()
            .map(|o| {
                let name = self.members.iter().find(|m| m.id == o.from).map(|m| m.name.as_str()).unwrap_or("Someone");
                json!({ "from": o.from, "name": name, "oid": o.oid, "kind": o.kind, "code": o.code, "title": o.title })
            })
            .collect();
        json!({ "op": "offers", "list": list })
    }

    pub(super) fn list(&self) -> Value {
        json!(self
            .members
            .iter()
            .map(|m| json!({ "id": m.id, "name": m.name, "host": m.host, "path": m.path, "rtt": m.rtt, "code": m.code }))
            .collect::<Vec<_>>())
    }

    pub(super) fn broadcast_members(&self) {
        self.broadcast(&json!({ "op": "members", "list": self.list() }));
    }

    /// Takes someone out: their connection closes, what they had open goes,
    /// everyone else hears.
    pub(super) fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.members.iter().position(|m| m.id == id) else { return false };
        let m = self.members.remove(i);
        let _ = m.stream.shutdown(Shutdown::Both);
        self.broadcast_members();
        let had = self.offers.len();
        self.offers.retain(|o| o.from != id);
        if self.offers.len() != had {
            self.broadcast(&self.offers_msg());
        }
        true
    }
}

/// A hub running on this PC.
pub struct HubServer {
    pub port: u16,
    /// Proves the host's own KittyConnect is the host. Never leaves this PC.
    pub host_token: String,
    pub(super) state: Arc<Mutex<State>>,
    pub(super) stop: Arc<AtomicBool>,
}

impl HubServer {
    /// Starts a hub's server: on a new free port for a temporary hub, or on
    /// a saved hub's own port (its code names it).
    pub fn start(meta: Meta, port: Option<u16>) -> Result<HubServer, String> {
        let (v4, v6) = match port {
            None => crate::loopback::listen().map_err(|e| format!("Couldn't open the hub: {e}"))?,
            Some(p) => listen_on(p)?,
        };
        let port = v4.local_addr().map_err(|e| e.to_string())?.port();
        let meta = Meta { name: clean(&meta.name, 48), ..meta };
        let state = Arc::new(Mutex::new(State {
            meta,
            members: Vec::new(),
            next_id: 1,
            next_inv: 1,
            book: Vec::new(),
            gone: Vec::new(),
            offers: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let host_token = crate::peers::secret();
        let open = Arc::new(AtomicUsize::new(0));
        for listener in std::iter::once(v4).chain(v6) {
            let (state, stop, token, open) = (state.clone(), stop.clone(), host_token.clone(), open.clone());
            std::thread::spawn(move || accept_loop(listener, state, stop, token, open));
        }
        Ok(HubServer { port, host_token, state, stop })
    }

    /// The host is closing a saved hub: whoever has been in it longest, of
    /// those who saved it, takes over with their own copy, and everyone hears
    /// where to go. Returns who, if anyone.
    pub fn hand_over(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.meta.expires?;
        let next = st.members.iter().filter(|m| !m.host && m.code.is_some()).min_by_key(|m| m.id)?;
        let (name, code) = (next.name.clone(), next.code.clone()?);
        st.broadcast(&json!({ "op": "moving", "code": code, "name": name }));
        Some(name)
    }

    /// Another copy of this saved hub is open too, and it's where everyone
    /// should be: everyone here hears to go there (`name`'s, at `code`).
    pub fn send_moving(&self, code: &str, name: &str) {
        let st = self.state.lock().unwrap();
        st.broadcast(&json!({ "op": "moving", "code": code, "name": name }));
    }

    /// How many are in the hub (the host included).
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.state.lock().unwrap().members.len()
    }
}

impl Drop for HubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        for m in st.members.drain(..) {
            let _ = m.stream.shutdown(Shutdown::Both);
        }
        drop(st);
        // Wake both accept loops so they see the flag.
        let wake = Duration::from_millis(300);
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), wake);
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv6Addr::LOCALHOST, self.port)), wake);
    }
}

/// A saved hub's own port, on 127.0.0.1 and (where there's IPv6) ::1. If
/// another program has it, the hub can't open under its code.
pub(super) fn listen_on(port: u16) -> Result<(TcpListener, Option<TcpListener>), String> {
    let busy = || format!("Another program is using port {port}, which this hub's code needs. Close it, or make a new hub.");
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|_| busy())?;
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(l) => Some(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(busy()),
        Err(_) => None,
    };
    Ok((v4, v6))
}

/// Connections open at once, members and ones still saying hello; more are
/// closed straight away (someone with the code opening lots of them).
pub(super) const MAX_OPEN: usize = MAX_MEMBERS * 2;

pub(super) fn accept_loop(listener: TcpListener, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>, token: String, open: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let (state, stop, token, open) = (state.clone(), stop.clone(), token.clone(), open.clone());
        std::thread::spawn(move || {
            let _ = serve_member(stream, &state, &stop, &token);
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

pub(super) fn serve_member(stream: TcpStream, state: &Mutex<State>, stop: &AtomicBool, token: &str) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(HELLO_WAIT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(hello) = read_line(&mut reader) else { return Ok(()) };
    // Scan: is this hub open, and whose is it? One line back, then done.
    if hello["op"] == "peek" {
        let st = state.lock().unwrap();
        let host = st.members.iter().find(|m| m.host).map(|m| m.name.clone()).unwrap_or_default();
        let mut reply = json!({
            "op": "peeked", "v": PROTOCOL, "id": st.meta.id, "hub": st.meta.name, "host": host,
            "members": st.members.len(), "expiresIn": st.expires_in(),
        });
        // A saved hub's book goes with it, so whoever scans catches up.
        if st.meta.expires.is_some() {
            let b = st.book_msg();
            for k in ["book", "gone", "goneAt"] {
                reply[k] = b[k].clone();
            }
        }
        // Another copy of this saved hub is open too, and looking at us: our
        // host's app hears, so it looks back (it checks for itself; this is
        // only a hint).
        if let (true, Some(code)) = (st.meta.expires.is_some(), hubstore::clean_code(hello["openAt"].as_str().unwrap_or_default())) {
            if let Some(h) = st.members.iter().find(|m| m.host) {
                let _ = h.out.send(json!({ "op": "alsoOpen", "code": code }));
            }
        }
        drop(st);
        send_line(&mut writer, &reply);
        return Ok(());
    }
    if hello["op"] != "hello" {
        return Ok(());
    }
    if hello["v"].as_u64().unwrap_or(0) != PROTOCOL {
        send_line(&mut writer, &json!({ "op": "old", "v": PROTOCOL }));
        return Ok(());
    }
    let mut name = clean(hello["name"].as_str().unwrap_or(""), crate::peers::MAX_NAME);
    if name.is_empty() {
        name = "Someone".into();
    }
    let peer = hello["peer"].as_u64().unwrap_or(0);
    let claims_host = hello["host"].as_str().is_some_and(|t| same_secret(t, token));

    let (out_tx, out_rx) = channel::<Value>();
    let id = {
        let mut st = state.lock().unwrap();
        // The same person reconnecting replaces their old entry.
        if peer != 0 {
            if let Some(old) = st.members.iter().find(|m| m.peer == peer).map(|m| m.id) {
                st.remove(old);
            }
        }
        if st.members.len() >= MAX_MEMBERS {
            drop(st);
            send_line(&mut writer, &json!({ "op": "full" }));
            return Ok(());
        }
        let host = claims_host && !st.members.iter().any(|m| m.host);
        let id = st.next_id;
        st.next_id += 1;
        st.members.push(Member {
            id,
            peer,
            name: name.clone(),
            host,
            path: if host { "host" } else { path_of(&hello) },
            rtt: rtt_of(&hello),
            out: out_tx.clone(),
            stream: stream.try_clone()?,
            invites: VecDeque::new(),
            code: None,
        });
        let _ = out_tx.send(json!({
            "op": "welcome", "v": PROTOCOL, "you": id, "hub": st.meta.name, "id": st.meta.id,
            "expiresIn": st.expires_in(), "host": host,
        }));
        st.broadcast_members();
        let _ = out_tx.send(st.book_msg());
        let _ = out_tx.send(st.offers_msg());
        id
    };

    // Everything to this member goes through one writer, in order.
    let writer_thread = std::thread::spawn(move || {
        while let Ok(v) = out_rx.recv() {
            if !send_line(&mut writer, &v) {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });

    stream.set_read_timeout(Some(SILENT_FOR))?;
    while !stop.load(Ordering::Relaxed) {
        let Some(msg) = read_line(&mut reader) else { break };
        match msg["op"].as_str().unwrap_or_default() {
            "ping" => {
                let _ = out_tx.send(json!({ "op": "pong" }));
            }
            "path" => {
                let mut st = state.lock().unwrap();
                let Some(m) = st.members.iter_mut().find(|m| m.id == id) else { break };
                if m.host {
                    continue;
                }
                let (path, rtt) = (path_of(&msg), rtt_of(&msg));
                if m.path != path || m.rtt != rtt {
                    m.path = path;
                    m.rtt = rtt;
                    st.broadcast_members();
                }
            }
            "name" => {
                let name = clean(msg["name"].as_str().unwrap_or(""), crate::peers::MAX_NAME);
                let mut st = state.lock().unwrap();
                if let Some(m) = st.members.iter_mut().find(|m| m.id == id).filter(|m| !name.is_empty() && m.name != name) {
                    m.name = name.clone();
                    let code = m.code.clone();
                    st.broadcast_members();
                    // A saved hub's book has their new name, as of now.
                    if let (Some(code), true) = (code, st.meta.expires.is_some()) {
                        let st = &mut *st;
                        let at = st.stamp(&code);
                        if let Some(e) = hubstore::entry(&name, &code, at) {
                            if hubstore::merge(&mut st.book, &mut st.gone, &[e], &[]) {
                                st.broadcast(&st.book_msg());
                            }
                        }
                    }
                }
            }
            "invite" => {
                let reply = invite(state, id, &msg);
                let _ = out_tx.send(reply);
            }
            // Saved hubs: everyone's codes for it, so anyone can scan for it.
            "address" | "merge" | "forget" => {
                let mut st = state.lock().unwrap();
                if st.meta.expires.is_none() {
                    continue;
                }
                let st = &mut *st;
                let changed = match msg["op"].as_str() {
                    // Their code, as of now (saving again after a delete brings it back).
                    Some("address") => {
                        let name = st.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                        let code = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()).unwrap_or_default();
                        let at = st.stamp(&code);
                        match hubstore::entry(&name, &code, at) {
                            Some(e) => {
                                // Everyone's member list says who could take over.
                                let new_code = st.members.iter_mut().find(|m| m.id == id).filter(|m| m.code.as_ref() != Some(&e.code)).map(|m| {
                                    m.code = Some(e.code.clone());
                                });
                                if new_code.is_some() {
                                    st.broadcast_members();
                                }
                                hubstore::merge(&mut st.book, &mut st.gone, &[e], &[])
                            }
                            None => false,
                        }
                    }
                    // Their copy of the book: newest wins, code by code.
                    Some("merge") => {
                        let book: Vec<Entry> = serde_json::from_value(msg["book"].clone()).unwrap_or_default();
                        let gone = hubstore::gone_from(&msg);
                        hubstore::merge(&mut st.book, &mut st.gone, &book, &gone)
                    }
                    // They deleted their copy.
                    _ => {
                        let code = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()).unwrap_or_default();
                        let at = st.stamp(&code);
                        hubstore::merge(&mut st.book, &mut st.gone, &[], &[Gone { code, at }])
                    }
                };
                if changed {
                    st.broadcast(&st.book_msg());
                }
            }
            // "Open to hub": something we have open, or not any more.
            "offer" | "withdraw" => {
                let mut st = state.lock().unwrap();
                let oid: String = clean(msg["oid"].as_str().unwrap_or_default(), 64);
                if oid.is_empty() {
                    continue;
                }
                st.offers.retain(|o| !(o.from == id && o.oid == oid));
                if msg["op"] == "offer" {
                    let mine = st.offers.iter().filter(|o| o.from == id).count();
                    if let (Some((kind, code)), true) = (invitation(msg["code"].as_str().unwrap_or_default()), mine < MAX_OFFERS_EACH) {
                        let title = clean(msg["title"].as_str().unwrap_or(""), MAX_TITLE);
                        st.offers.push(Offer { from: id, oid, kind, code, title });
                    }
                }
                st.broadcast(&st.offers_msg());
            }
            // Anyone in it can delete a saved hub for everyone in it now.
            "delete" => {
                let st = state.lock().unwrap();
                let by = st.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                st.broadcast(&json!({ "op": "deleted", "by": by, "id": st.meta.id }));
            }
            "kick" => {
                let mut st = state.lock().unwrap();
                let is_host = st.members.iter().any(|m| m.id == id && m.host);
                let target = msg["id"].as_u64().filter(|t| *t != id);
                if let (true, Some(t)) = (is_host, target) {
                    if let Some(m) = st.members.iter().find(|m| m.id == t) {
                        let _ = m.out.send(json!({ "op": "kicked" }));
                        // Give the writer a moment to deliver it.
                        let s = m.stream.try_clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(300));
                            if let Ok(s) = s {
                                let _ = s.shutdown(Shutdown::Both);
                            }
                        });
                    }
                    st.members.retain(|m| m.id != t);
                    st.broadcast_members();
                }
            }
            "bye" => break,
            _ => {}
        }
    }
    {
        let mut st = state.lock().unwrap();
        st.remove(id);
    }
    drop(out_tx);
    let _ = writer_thread.join();
    Ok(())
}

pub(super) fn path_of(msg: &Value) -> &'static str {
    match msg["path"].as_str() {
        Some("direct") => "direct",
        Some("relay") => "relay",
        _ => "unknown",
    }
}

pub(super) fn rtt_of(msg: &Value) -> Option<u32> {
    msg["rtt"].as_u64().map(|r| r.min(60_000) as u32)
}

/// Passes an invite on to the people it's for. The code must be one the hub
/// can invite to; what it's for comes from the code itself, not from what
/// the sender says it is.
pub(super) fn invite(state: &Mutex<State>, from: u64, msg: &Value) -> Value {
    let Some((kind, code_clean)) = invitation(msg["code"].as_str().unwrap_or_default()) else {
        return json!({ "op": "invited", "n": 0, "error": "That can't be shared in a hub." });
    };
    let title = clean(msg["title"].as_str().unwrap_or(""), MAX_TITLE);
    let to: Vec<u64> = msg["to"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let mut st = state.lock().unwrap();
    let now = Instant::now();
    let Some(sender) = st.members.iter_mut().find(|m| m.id == from) else {
        return json!({ "op": "invited", "n": 0 });
    };
    while sender.invites.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
        sender.invites.pop_front();
    }
    if sender.invites.len() >= INVITES_PER_MINUTE {
        return json!({ "op": "invited", "n": 0, "error": "That's a lot of invites. Wait a minute and try again." });
    }
    sender.invites.push_back(now);
    let name = sender.name.clone();
    let inv = st.next_inv;
    st.next_inv += 1;
    let out = json!({ "op": "invite", "inv": inv, "from": from, "name": name, "kind": kind, "code": code_clean, "title": title });
    let mut n = 0;
    for m in st.members.iter().filter(|m| m.id != from && (to.is_empty() || to.contains(&m.id))) {
        if m.out.send(out.clone()).is_ok() {
            n += 1;
        }
    }
    json!({ "op": "invited", "n": n })
}
