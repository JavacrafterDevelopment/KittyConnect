use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;

use super::*;

struct Game {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Game {
    fn connect(port: u16) -> Game {
        let s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut g = Game { w: s.try_clone().unwrap(), r: BufReader::new(s) };
        let hi = g.next();
        assert_eq!((hi["op"].as_str(), hi["v"].as_u64()), (Some("portside"), Some(VERSION)));
        g
    }
    fn send_raw(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).unwrap();
        self.w.write_all(b"\n").unwrap();
    }
    fn send(&mut self, v: Value) {
        self.send_raw(&v.to_string());
    }
    fn hello(&mut self, token: Option<&str>) {
        let mut h = json!({ "op": "hello", "v": 1, "app": { "id": "space-game", "name": "Space Game" } });
        if let Some(t) = token {
            h["token"] = json!(t);
        }
        self.send(h);
    }
    /// The next line, or Null when the connection's closed.
    fn next(&mut self) -> Value {
        let mut l = String::new();
        match self.r.read_line(&mut l) {
            Ok(n) if n > 0 => serde_json::from_str(&l).unwrap(),
            _ => Value::Null,
        }
    }
}

/// The app's side: an Api, and its events applied as the app's loop would.
struct Host {
    api: Api,
    rx: Receiver<ApiEvent>,
}

impl Host {
    fn new() -> Host {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let emit: Emit = Arc::new(move |ev| {
            let _ = tx.lock().unwrap().send(ev);
        });
        Host { api: Api::new(Config { enabled: true, apps: Vec::new() }, emit), rx }
    }
    fn port(&self) -> u16 {
        self.api.port().expect("listening")
    }
    /// Takes in what's arrived, waiting a moment for at least one.
    fn pump(&mut self) -> Vec<u64> {
        let mut conns = Vec::new();
        let mut wait = Duration::from_secs(5);
        while let Ok(ev) = self.rx.recv_timeout(wait) {
            wait = Duration::from_millis(150);
            match ev {
                ApiEvent::Open { conn, tx, exe, hello } => {
                    conns.push(conn);
                    self.api.open(conn, tx, exe, &hello);
                }
                ApiEvent::Closed { conn } => self.api.closed(conn),
                ApiEvent::Msg { conn, msg } => {
                    if let Some((app, _)) = self.api.who(conn, &msg) {
                        self.api.reply(conn, ok(&msg["id"], json!({ "app": app })));
                    }
                }
            }
        }
        conns
    }
}

#[test]
fn game_ids() {
    assert_eq!(app_id("com.kit.space-game_2").as_deref(), Some("com.kit.space-game_2"));
    for bad in ["", "Space", "-game", "a b", "spåce", &"a".repeat(65)] {
        assert_eq!(app_id(bad), None, "{bad}");
    }
}

/// Asked once, allowed, then let straight in with its key; a wrong key asks
/// again; "Don't allow" turns it away for a minute; removing it in Settings
/// disconnects it. Only 127.0.0.1 hears any of it, and only JSON.
#[test]
fn a_game_is_asked_about_then_allowed_with_its_own_key() {
    let mut host = Host::new();
    let port = host.port();
    assert!(PORTS.contains(&port));
    // Nothing but this PC can connect: the listener is on 127.0.0.1 alone,
    // so this PC's own network address doesn't answer.
    let lan = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok().and_then(|u| {
        u.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
        u.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    });
    if let Some(ip) = lan {
        assert!(TcpStream::connect_timeout(&std::net::SocketAddr::new(ip, port), Duration::from_secs(2)).is_err(), "reachable at {ip}");
    }

    // A browser's request isn't a hello: closed at once.
    let mut web = Game::connect(port);
    web.send_raw("GET / HTTP/1.1");
    assert!(web.next().is_null());

    let mut g = Game::connect(port);
    g.hello(None);
    let conns = host.pump();
    assert_eq!(g.next()["op"], "asking");
    let prompt = host.api.json()["prompt"].clone();
    assert_eq!((prompt["name"].as_str(), prompt["id"].as_str()), (Some("Space Game"), Some("space-game")));
    // Requests before it's allowed are refused.
    g.send(json!({ "id": 1, "cmd": "getHub" }));
    host.pump();
    assert_eq!(g.next()["ok"], false);
    host.api.allow(conns[0]);
    let a = g.next();
    assert_eq!(a["op"], "authorized");
    let token = a["token"].as_str().unwrap().to_string();
    assert_eq!(g.next()["op"], "ready");
    // Only a hash of the key is kept.
    assert_eq!(host.api.config.apps.len(), 1);
    assert_ne!(host.api.config.apps[0].hash, token);
    assert_eq!(host.api.config.apps[0].hash, sha256::hex(token.as_bytes()));
    g.send(json!({ "id": "x", "cmd": "anything" }));
    host.pump();
    assert_eq!(g.next(), json!({ "id": "x", "ok": true, "data": { "app": "space-game" } }));
    assert!(host.api.json()["prompt"].is_null());
    assert_eq!(host.api.json()["apps"][0]["connected"], 1);

    // Next time: straight in.
    let mut g2 = Game::connect(port);
    g2.hello(Some(&token));
    host.pump();
    assert_eq!(g2.next()["op"], "ready");

    // A made-up key: asked again; "Don't allow" turns it away, and it can't ask for a while.
    let mut g3 = Game::connect(port);
    g3.hello(Some("not-the-key"));
    let c3 = host.pump();
    assert_eq!(g3.next()["op"], "asking");
    host.api.deny(c3[0]);
    assert_eq!(g3.next()["op"], "denied");
    assert!(g3.next().is_null());
    let mut g4 = Game::connect(port);
    g4.hello(None);
    host.pump();
    assert_eq!(g4.next()["op"], "denied");
    // ...but the real key still works.
    let mut g5 = Game::connect(port);
    g5.hello(Some(&token));
    host.pump();
    assert_eq!(g5.next()["op"], "ready");

    // Removed in Settings: everyone connected as it is disconnected, and its key stops working.
    host.api.remove("space-game");
    for g in [&mut g, &mut g2, &mut g5] {
        assert_eq!(g.next()["op"], "denied");
        assert!(g.next().is_null());
    }
    host.pump();
    assert!(!host.api.has_ready());

    // Turned off: the port closes.
    host.api.set_enabled(false);
    std::thread::sleep(Duration::from_millis(200));
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map(|mut s| {
        let mut b = [0u8; 1];
        std::io::Read::read(&mut s, &mut b).unwrap_or(0) == 0
    }).unwrap_or(true));
}

/// Events go only when something changed, and game messages reach only the
/// same game.
#[test]
fn events_go_once_and_messages_reach_the_same_game() {
    let mut host = Host::new();
    let port = host.port();
    let mut g = Game::connect(port);
    g.hello(None);
    let c = host.pump();
    g.next();
    host.api.allow(c[0]);
    g.next();
    g.next();
    host.api.push("hub", |_| json!({ "name": "Kit's hub" }));
    host.api.push("hub", |_| json!({ "name": "Kit's hub" }));
    host.api.push("hub", |_| Value::Null);
    assert_eq!(g.next(), json!({ "evt": "hub", "data": { "name": "Kit's hub" } }));
    assert_eq!(g.next(), json!({ "evt": "hub", "data": null }));
    host.api.deliver(&json!({ "op": "app", "app": "other-game", "from": 2, "name": "Jo", "data": 1 }));
    host.api.deliver(&json!({ "op": "app", "app": "space-game", "from": 2, "name": "Jo", "data": { "go": true } }));
    assert_eq!(g.next(), json!({ "evt": "message", "data": { "from": { "id": 2, "name": "Jo" }, "data": { "go": true } } }));
    host.api.set_activity(c[0], Some("In a lobby".into()));
    assert_eq!(host.api.activity(), Some(("Space Game".into(), "In a lobby".into())));
    drop(g);
    host.pump();
    assert_eq!(host.api.activity(), None);
}
