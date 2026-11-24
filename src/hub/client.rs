//! One connection to a hub (and Scan's one-line peek at one).

use super::*;

pub enum HubEvent {
    /// Connected and welcomed: our id, the hub's name, id and time left (a
    /// saved hub), whether we're its host.
    Welcome { you: u64, hub: String, id: String, expires_in: Option<u64>, host: bool, games: bool },
    /// A saved hub's address book, and the codes deleted from it.
    Book { book: Vec<Entry>, gone: Vec<Gone> },
    /// What everyone has open in the hub ("Open to hub").
    Offers(Vec<Value>),
    /// Game sessions everyone's games host (the game API).
    Sessions(Vec<Value>),
    /// A game message from someone's game (the game API), as the hub sent it.
    App(Value),
    /// Someone deleted the hub for everyone.
    Deleted { by: String },
    /// The host closed the saved hub: go to this code, `name`'s copy.
    Moving { code: String, name: String },
    /// Our saved hub is open at `code` too (that copy just looked at ours).
    AlsoOpen { code: String },
    /// How long the hub took to answer a ping, in ms.
    Rtt(u32),
    Members(Vec<Value>),
    Invite(Value),
    /// How many an invite reached, or why none.
    Invited { n: u64, error: String },
    /// Lost the connection; trying again.
    Reconnecting,
    /// The host removed us.
    Kicked,
    Full,
    /// The hub runs a different version of KittyConnect.
    Old,
}

/// Our connection to a hub (ours or a friend's), kept up until dropped.
pub struct HubConn {
    pub(super) tx: Sender<Value>,
    pub(super) hello: Arc<Mutex<Value>>,
    pub(super) stop: Arc<AtomicBool>,
}

impl HubConn {
    pub fn send(&self, v: Value) {
        // The latest path also goes in the hello, so a reconnect shows it at once.
        if v["op"] == "path" || v["op"] == "name" {
            let mut h = self.hello.lock().unwrap();
            for k in ["path", "rtt", "name"] {
                if let Some(x) = v.get(k) {
                    h[k] = x.clone();
                }
            }
        }
        let _ = self.tx.send(v);
    }
}

impl Drop for HubConn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(json!({ "op": "bye" }));
    }
}

/// Connects to the hub at 127.0.0.1:`port` (its server, or the near end of
/// the tunnel to it) and keeps the connection up, reconnecting if it drops.
/// `hello` is sent each time (name, path, and the host secret for our own hub).
/// What it hears goes to `emit`.
pub fn connect(port: u16, hello: Value, emit: impl Fn(HubEvent) + Send + 'static) -> HubConn {
    let (tx, rx) = channel::<Value>();
    let stop = Arc::new(AtomicBool::new(false));
    let hello = Arc::new(Mutex::new(hello));
    let (flag, h) = (stop.clone(), hello.clone());
    std::thread::spawn(move || client_loop(port, h, rx, flag, &emit));
    HubConn { tx, hello, stop }
}

/// Our hub connection's events, to the app's event loop.
pub(super) fn to_app(gen: u64, proxy: &EventLoopProxy<UserEvent>) -> impl Fn(HubEvent) + Send + 'static {
    let proxy = proxy.clone();
    move |ev| {
        let _ = proxy.send_event(UserEvent::Hub(gen, ev));
    }
}

pub(super) fn client_loop(port: u16, hello: Arc<Mutex<Value>>, rx: Receiver<Value>, stop: Arc<AtomicBool>, emit: &dyn Fn(HubEvent)) {
    let rx = Arc::new(Mutex::new(rx));
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !first {
            emit(HubEvent::Reconnecting);
            std::thread::sleep(Duration::from_secs(2));
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        first = false;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else { continue };
        stream.set_nodelay(true).ok();
        let hello = hello.lock().unwrap().clone();
        match session(stream, &hello, &rx, &stop, emit) {
            End::Lost => continue,
            End::Stopped => break,
            End::Final(ev) => {
                emit(ev);
                break;
            }
        }
    }
}

pub(super) enum End {
    /// The connection dropped: try again.
    Lost,
    /// We left.
    Stopped,
    /// The hub won't have us (removed, full, another version).
    Final(HubEvent),
}

pub(super) fn session(stream: TcpStream, hello: &Value, rx: &Arc<Mutex<Receiver<Value>>>, stop: &AtomicBool, emit: &dyn Fn(HubEvent)) -> End {
    let (Ok(mut writer), Ok(read_half)) = (stream.try_clone(), stream.try_clone()) else { return End::Lost };
    let mut reader = BufReader::new(read_half);
    if !send_line(&mut writer, hello) {
        return End::Lost;
    }
    stream.set_read_timeout(Some(HELLO_WAIT)).ok();
    let Some(welcome) = read_line(&mut reader) else { return End::Lost };
    match welcome["op"].as_str() {
        Some("welcome") => emit(HubEvent::Welcome {
            you: welcome["you"].as_u64().unwrap_or(0),
            hub: clean(welcome["hub"].as_str().unwrap_or(""), 48),
            id: welcome["id"].as_str().filter(|i| hubstore::is_id(i)).unwrap_or_default().to_string(),
            expires_in: welcome["expiresIn"].as_u64().map(|s| s.min(hubstore::MAX_LIFE)),
            host: welcome["host"] == true,
            games: welcome["games"].as_u64().unwrap_or(0) >= 1,
        }),
        Some("full") => return End::Final(HubEvent::Full),
        Some("old") => return End::Final(HubEvent::Old),
        _ => return End::Lost,
    }
    stream.set_read_timeout(Some(SILENT_FOR)).ok();

    // Our messages and pings go out from here (anything queued while we were
    // reconnecting goes first); the hub's come in below.
    let done = Arc::new(AtomicBool::new(false));
    // When the last ping went, to time its pong.
    let pinged: Arc<Mutex<Option<Instant>>> = Arc::default();
    let (rx2, done2, pinged2) = (rx.clone(), done.clone(), pinged.clone());
    let sender = std::thread::spawn(move || {
        // The first ping goes soon, so the ping time shows quickly.
        let mut last_ping = Instant::now() - PING_EVERY + Duration::from_secs(2);
        let mut said_bye = false;
        while !done2.load(Ordering::Relaxed) {
            let next = rx2.lock().ok().and_then(|q| q.recv_timeout(Duration::from_millis(500)).ok());
            if let Some(v) = next {
                said_bye = v["op"] == "bye";
                if !send_line(&mut writer, &v) || said_bye {
                    break;
                }
            }
            if last_ping.elapsed() >= PING_EVERY {
                last_ping = Instant::now();
                *pinged2.lock().unwrap() = Some(last_ping);
                if !send_line(&mut writer, &json!({ "op": "ping" })) {
                    break;
                }
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
        said_bye
    });

    let mut end = End::Lost;
    while let Some(msg) = read_line(&mut reader) {
        match msg["op"].as_str().unwrap_or_default() {
            "members" => {
                let list = msg["list"].as_array().cloned().unwrap_or_default();
                emit(HubEvent::Members(list.into_iter().take(MAX_MEMBERS).collect()));
            }
            "invite" => emit(HubEvent::Invite(msg)),
            "book" => {
                let (book, gone) = read_book(&msg);
                emit(HubEvent::Book { book, gone });
            }
            "offers" => emit(HubEvent::Offers(msg["list"].as_array().cloned().unwrap_or_default())),
            "sessions" => emit(HubEvent::Sessions(msg["list"].as_array().cloned().unwrap_or_default())),
            "app" => emit(HubEvent::App(msg)),
            "deleted" => emit(HubEvent::Deleted { by: clean(msg["by"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) }),
            "moving" => {
                if let Some(code) = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()) {
                    emit(HubEvent::Moving { code, name: clean(msg["name"].as_str().unwrap_or("Someone"), crate::peers::MAX_NAME) });
                }
            }
            "alsoOpen" => {
                if let Some(code) = hubstore::clean_code(msg["code"].as_str().unwrap_or_default()) {
                    emit(HubEvent::AlsoOpen { code });
                }
            }
            "pong" => {
                if let Some(t) = pinged.lock().unwrap().take() {
                    emit(HubEvent::Rtt(t.elapsed().as_millis().min(60_000) as u32));
                }
            }
            "invited" => emit(HubEvent::Invited {
                n: msg["n"].as_u64().unwrap_or(0),
                error: clean(msg["error"].as_str().unwrap_or(""), 120),
            }),
            "kicked" => {
                end = End::Final(HubEvent::Kicked);
                break;
            }
            _ => {}
        }
    }
    done.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
    let said_bye = sender.join().unwrap_or(false);
    match end {
        End::Final(_) => end,
        _ if said_bye || stop.load(Ordering::Relaxed) => End::Stopped,
        _ => End::Lost,
    }
}

/// A book and its deletions as they arrive, checked: hub codes only, names
/// cleaned, within the limits.
pub(super) fn read_book(msg: &Value) -> (Vec<Entry>, Vec<Gone>) {
    let book = serde_json::from_value::<Vec<Entry>>(msg["book"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| hubstore::entry(&e.name, &e.code, e.at))
        .take(hubstore::MAX_BOOK)
        .collect();
    let gone = hubstore::gone_from(msg)
        .into_iter()
        .filter_map(|g| Some(Gone { code: hubstore::clean_code(&g.code)?, at: g.at }))
        .take(hubstore::MAX_GONE)
        .collect();
    (book, gone)
}

/// What a scan found at one code: an open copy of a hub, and its book.
#[derive(Clone, Debug)]
pub struct Peeked {
    pub id: String,
    pub hub: String,
    pub host: String,
    pub members: u64,
    pub book: Vec<Entry>,
    pub gone: Vec<Gone>,
}

/// Asks whatever answers at 127.0.0.1:`port` (the near end of a tunnel to
/// someone's copy of a hub) whether it's open, without joining. Waits up to
/// `wait`: the tunnel comes up first, and a copy that isn't open never answers.
#[cfg(test)]
pub fn peek(port: u16, wait: Duration) -> Result<Peeked, String> {
    peek_from(port, wait, None, &AtomicBool::new(false))
}

/// A peek, from a copy of the hub that's open itself (at `mine`) if given:
/// the copy we peek hears so, and looks back (two open copies join up).
///
/// TailCat takes our connection at once and only then reaches for the other
/// end; if that first try fails (their relay connection was still coming
/// up, say) it just closes ours. So we ask again, until `wait` is up, or
/// until `stopped` (then our TailCat is closed, which ends a wait at once).
pub(super) fn peek_from(port: u16, wait: Duration, mine: Option<&str>, stopped: &AtomicBool) -> Result<Peeked, String> {
    let until = Instant::now() + wait;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut ask = json!({ "op": "peek", "v": PROTOCOL });
    if let Some(code) = mine {
        ask["openAt"] = json!(code);
    }
    let v = loop {
        let left = until.saturating_duration_since(Instant::now());
        let answer = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok().and_then(|stream| {
            let mut writer = stream.try_clone().ok()?;
            stream.set_read_timeout(Some(left.max(Duration::from_secs(1)))).ok();
            if !send_line(&mut writer, &ask) {
                return None;
            }
            read_line(&mut BufReader::new(stream))
        });
        match answer {
            Some(v) => break v,
            None if stopped.load(Ordering::Relaxed) => return Err("stopped".into()),
            None if until.saturating_duration_since(Instant::now()) > Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(500))
            }
            None => return Err("not open".into()),
        }
    };
    if v["op"] != "peeked" {
        return Err("not open".into());
    }
    if v["v"].as_u64() != Some(PROTOCOL) {
        return Err("a different version of Portside".into());
    }
    let (book, gone) = read_book(&v);
    Ok(Peeked {
        book,
        gone,
        id: v["id"].as_str().filter(|i| hubstore::is_id(i)).unwrap_or_default().to_string(),
        hub: clean(v["hub"].as_str().unwrap_or(""), 48),
        host: clean(v["host"].as_str().unwrap_or(""), crate::peers::MAX_NAME),
        members: v["members"].as_u64().unwrap_or(0).min(MAX_MEMBERS as u64),
    })
}
