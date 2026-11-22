use super::*;

/// A raw client for the tests: hello, then lines.
struct Raw {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Raw {
    fn join(port: u16, hello: Value) -> Raw {
        let s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut raw = Raw { w: s.try_clone().unwrap(), r: BufReader::new(s) };
        raw.send(hello);
        raw
    }
    fn send(&mut self, v: Value) {
        assert!(send_line(&mut self.w, &v));
    }
    fn next(&mut self) -> Value {
        read_line(&mut self.r).unwrap_or(Value::Null)
    }
    /// The next message with this op, skipping others.
    fn wait(&mut self, op: &str) -> Value {
        for _ in 0..50 {
            let v = self.next();
            if v.is_null() || v["op"] == op {
                return v;
            }
        }
        Value::Null
    }
}

fn hello(name: &str, peer: u64) -> Value {
    json!({ "op": "hello", "v": PROTOCOL, "peer": peer, "name": name, "path": "direct", "rtt": 20 })
}

/// A temporary hub's server.
fn temporary(name: &str) -> HubServer {
    HubServer::start(Meta { id: hubstore::new_id(), name: name.into(), expires: None }, None).unwrap()
}

/// A saved hub's server, on its own port.
fn saved(name: &str) -> (HubServer, String) {
    let id = hubstore::new_id();
    let port = hubstore::pick_port().unwrap();
    let meta = Meta { id: id.clone(), name: name.into(), expires: Some(hubstore::now() + 7 * 24 * 3600) };
    let hub = HubServer::start(meta, Some(port)).unwrap();
    assert_eq!(hub.port, port);
    (hub, id)
}

fn hub_code(n: u8) -> String {
    code::hub(21000 + n as u16, &format!("tc{}", (b'a' + n) as char).repeat(30))
}

const VOIP: &str = "kitty:voip:50000:tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN";

#[test]
fn members_see_each_other_and_invites_reach_only_whom_they_are_for() {
    let hub = temporary("Kit's hub");
    let mut host = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    let w = host.wait("welcome");
    assert_eq!((w["you"].as_u64(), w["host"].as_bool(), w["hub"].as_str()), (Some(1), Some(true), Some("Kit's hub")));
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    assert_eq!(jo.wait("welcome")["host"], false);
    let mut sam = Raw::join(hub.port, hello("Sam", 3));
    sam.wait("welcome");
    // Everyone gets the full list, with how each is connected.
    let list = sam.wait("members");
    let list = list["list"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["path"], "host");
    assert_eq!(list[1]["path"], "direct");
    assert_eq!(hub.count(), 3);

    // An invite to Jo alone: Jo gets it, Sam doesn't.
    host.send(json!({ "op": "invite", "to": [2], "code": format!("`{VOIP}`"), "title": "Kit's room", "kind": "file" }));
    let inv = jo.wait("invite");
    assert_eq!(inv["name"], "Kit");
    // The kind comes from the code, not from what the sender said.
    assert_eq!(inv["kind"], "voip");
    assert_eq!(inv["code"], VOIP);
    assert_eq!(host.wait("invited")["n"], 1);
    // Everyone else, from Sam.
    sam.send(json!({ "op": "invite", "to": [], "code": VOIP, "title": "x" }));
    assert_eq!(sam.wait("invited")["n"], 2);
    assert_eq!(jo.wait("invite")["name"], "Sam");

    // Something that isn't a code a hub can invite to goes nowhere.
    jo.send(json!({ "op": "invite", "to": [], "code": "kitty:hub:1:tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN" }));
    assert_eq!(jo.wait("invited")["n"], 0);
    jo.send(json!({ "op": "invite", "to": [], "code": "rm -rf /" }));
    assert_eq!(jo.wait("invited")["n"], 0);
}

#[test]
fn only_the_host_can_remove_people_and_the_secret_is_needed_to_be_host() {
    let hub = temporary("h");
    // Claiming to be host with a wrong secret: just a member.
    let mut fake = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 9, "name": "Fake", "host": "0000" }));
    assert_eq!(fake.wait("welcome")["host"], false);
    let mut host = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    assert_eq!(host.wait("welcome")["host"], true);
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    let jo_id = jo.wait("welcome")["you"].as_u64().unwrap();
    // A member can't remove anyone.
    fake.send(json!({ "op": "kick", "id": jo_id }));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(hub.count(), 3);
    // The host can.
    host.send(json!({ "op": "kick", "id": jo_id }));
    assert_eq!(jo.wait("kicked")["op"], "kicked");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(hub.count(), 2);
    assert!(read_line(&mut jo.r).is_none(), "the connection is closed");
}

#[test]
fn a_reconnect_replaces_the_old_entry_and_an_old_version_is_told() {
    let hub = temporary("h");
    let mut a = Raw::join(hub.port, hello("Jo", 77));
    a.wait("welcome");
    let mut b = Raw::join(hub.port, hello("Jo", 77));
    b.wait("welcome");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(hub.count(), 1);
    let mut old = Raw::join(hub.port, json!({ "op": "hello", "v": 999, "name": "Old" }));
    assert_eq!(old.next()["op"], "old");
    // A line too long is the end of that connection.
    let mut big = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "name": "x".repeat(70_000) }));
    assert!(big.next().is_null());
}

#[test]
fn invites_are_rate_limited() {
    let hub = temporary("h");
    let mut a = Raw::join(hub.port, hello("A", 1));
    a.wait("welcome");
    let mut b = Raw::join(hub.port, hello("B", 2));
    b.wait("welcome");
    let mut reached = 0;
    for _ in 0..20 {
        a.send(json!({ "op": "invite", "to": [], "code": VOIP }));
        if a.wait("invited")["n"] == 1 {
            reached += 1;
        }
    }
    assert_eq!(reached, INVITES_PER_MINUTE);
}

/// The client the app uses, against the server: it's welcomed, sees the
/// others, gets an invite, comes back after the connection drops, and
/// stops for good when the host removes it.
#[test]
fn the_app_client_end_to_end() {
    use std::sync::mpsc::RecvTimeoutError;
    let hub = temporary("Kit's hub");
    let (tx, rx) = channel::<HubEvent>();
    let tx = Mutex::new(tx);
    let conn = connect(hub.port, hello("Jo", 42), move |ev| {
        let _ = tx.lock().unwrap().send(ev);
    });
    let next = |want: &str| -> HubEvent {
        loop {
            let ev = match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => panic!("no {want}"),
                Err(e) => panic!("{e}"),
            };
            let name = match &ev {
                HubEvent::Welcome { .. } => "welcome",
                HubEvent::Members(_) => "members",
                HubEvent::Invite(_) => "invite",
                HubEvent::Invited { .. } => "invited",
                HubEvent::Reconnecting => "reconnecting",
                HubEvent::Kicked => "kicked",
                HubEvent::Full => "full",
                HubEvent::Old => "old",
                HubEvent::Book { .. } => "book",
                HubEvent::Offers(_) => "offers",
                HubEvent::Deleted { .. } => "deleted",
                HubEvent::Rtt(_) => "rtt",
                HubEvent::Moving { .. } => "moving",
                HubEvent::AlsoOpen { .. } => "alsoOpen",
            };
            if name == want {
                return ev;
            }
        }
    };
    let HubEvent::Welcome { you, host, .. } = next("welcome") else { unreachable!() };
    assert!(!host);
    // The ping shown next to everyone: timed from the hub's answer.
    let HubEvent::Rtt(ms) = next("rtt") else { unreachable!() };
    assert!(ms < 1000);
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    loop {
        if let HubEvent::Members(list) = next("members") {
            if list.len() == 2 {
                break;
            }
        }
    }
    // The path we report shows up for everyone.
    conn.send(json!({ "op": "path", "path": "relay", "rtt": 80 }));
    loop {
        let m = kit.wait("members");
        if m["list"].as_array().unwrap().iter().any(|x| x["name"] == "Jo" && x["path"] == "relay") {
            break;
        }
    }
    kit.send(json!({ "op": "invite", "to": [you], "code": VOIP, "title": "Kit's room" }));
    let HubEvent::Invite(inv) = next("invite") else { unreachable!() };
    assert_eq!((inv["kind"].as_str(), inv["name"].as_str()), (Some("voip"), Some("Kit")));
    // We invite back.
    conn.send(json!({ "op": "invite", "to": [], "code": VOIP, "title": "x" }));
    let HubEvent::Invited { n, .. } = next("invited") else { unreachable!() };
    assert_eq!(n, 1);

    // The connection drops (the tunnel hiccups): it comes back by itself,
    // as the same person, still showing the path it last reported.
    {
        let st = hub.state.lock().unwrap();
        let m = st.members.iter().find(|m| m.name == "Jo").unwrap();
        let _ = m.stream.shutdown(Shutdown::Both);
    }
    next("reconnecting");
    let HubEvent::Welcome { you, .. } = next("welcome") else { unreachable!() };
    std::thread::sleep(Duration::from_millis(200));
    {
        let st = hub.state.lock().unwrap();
        assert_eq!(st.members.iter().filter(|m| m.name == "Jo").count(), 1);
        assert_eq!(st.members.iter().find(|m| m.name == "Jo").unwrap().path, "relay");
    }

    kit.send(json!({ "op": "kick", "id": you }));
    next("kicked");
    std::thread::sleep(Duration::from_millis(3000));
    assert_eq!(hub.count(), 1, "removed, and it doesn't come back by itself");
    drop(conn);
}

/// A saved hub's address book: each person's code, merged from everyone's
/// copies, and a deleted code stays out.
#[test]
fn a_saved_hub_shares_everyones_codes() {
    let (hub, id) = saved("Minecraft Hub");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    let w = kit.wait("welcome");
    assert_eq!((w["id"].as_str(), w["hub"].as_str()), (Some(id.as_str()), Some("Minecraft Hub")));
    let left = w["expiresIn"].as_u64().unwrap();
    assert!(left > 7 * 24 * 3600 - 60 && left <= 7 * 24 * 3600, "time left, not a date");
    assert_eq!(kit.wait("book")["book"], json!([]), "a new hub's book starts empty");
    kit.send(json!({ "op": "address", "code": hub_code(1) }));
    assert_eq!(kit.wait("book")["book"][0]["name"], "Kit");

    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    let b = jo.wait("book");
    assert_eq!(b["book"].as_array().unwrap().len(), 1, "a newcomer gets the book");
    // Jo's own code, and Jo's copy of the book (with someone Kit didn't know).
    jo.send(json!({ "op": "address", "code": hub_code(2) }));
    jo.send(json!({ "op": "merge", "book": [{ "name": "Sam", "code": hub_code(3) }, { "name": "Bad", "code": VOIP }], "gone": [] }));
    let mut book = Value::Null;
    for _ in 0..3 {
        book = kit.wait("book");
        if book["book"].as_array().unwrap().len() == 3 {
            break;
        }
    }
    let names: Vec<&str> = book["book"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Kit", "Jo", "Sam"], "not a room code, only hub codes");
    // Sam deleted their copy: their code leaves, and an old book can't bring it back.
    jo.send(json!({ "op": "forget", "code": hub_code(3) }));
    let b = kit.wait("book");
    assert_eq!(b["book"].as_array().unwrap().len(), 2);
    assert_eq!(b["gone"][0], hub_code(3));
    kit.send(json!({ "op": "merge", "book": [{ "name": "Sam", "code": hub_code(3) }], "gone": [] }));
    jo.send(json!({ "op": "ping" }));
    assert_eq!(jo.wait("pong")["op"], "pong");
    assert_eq!(hub.state.lock().unwrap().book.len(), 2);
}

/// Back after weeks with an old book: joining any open copy brings in
/// everyone added since, and nothing older than what the hub has wins.
/// A rename updates the book; a peek (Scan) carries it too.
#[test]
fn coming_back_after_a_while_catches_up() {
    let (hub, _) = saved("h");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    kit.send(json!({ "op": "address", "code": hub_code(1) }));
    // While we were away: Sam joined and saved it, Jo renamed.
    kit.send(json!({ "op": "merge", "book": [
        { "name": "Sam", "code": hub_code(3), "at": hubstore::now() - 100 },
        { "name": "Jo (new)", "code": hub_code(2), "at": hubstore::now() - 50 },
    ], "goneAt": [] }));
    for _ in 0..3 {
        if kit.wait("book")["book"].as_array().unwrap().len() == 3 {
            break;
        }
    }
    // We come back with our old book: Jo's old name, from weeks ago.
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    let got = jo.wait("book");
    let names: Vec<&str> = got["book"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Sam"), "we get who joined while we were away");
    jo.send(json!({ "op": "merge", "book": [{ "name": "Jo", "code": hub_code(2), "at": hubstore::now() - 14 * 24 * 3600 }], "goneAt": [] }));
    jo.send(json!({ "op": "ping" }));
    jo.wait("pong");
    let names: Vec<String> = hub.state.lock().unwrap().book.iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&"Jo (new)".to_string()), "an old book doesn't bring an old name back: {names:?}");
    // Jo says their own code, then renames: the book follows.
    jo.send(json!({ "op": "address", "code": hub_code(2) }));
    jo.send(json!({ "op": "name", "name": "Joanna" }));
    for _ in 0..5 {
        let b = kit.wait("book");
        if b["book"].as_array().unwrap().iter().any(|e| e["name"] == "Joanna") {
            break;
        }
    }
    // Scan's peek carries the book, with times, and the deletions both ways.
    let p = peek(hub.port, Duration::from_secs(5)).unwrap();
    assert_eq!(p.book.len(), 3);
    assert!(p.book.iter().all(|e| e.at > 0));
    assert!(p.book.iter().any(|e| e.name == "Joanna"));
}

/// Closing a saved hub hands it to whoever has been in it longest, of
/// those who saved it, and tells everyone where to go.
#[test]
fn closing_hands_the_hub_over() {
    let (hub, _) = saved("h");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    kit.send(json!({ "op": "address", "code": hub_code(1) }));
    // Jo came first but never saved it; Sam saved it.
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    let mut sam = Raw::join(hub.port, hello("Sam", 3));
    sam.wait("welcome");
    sam.send(json!({ "op": "address", "code": hub_code(3) }));
    // The member list carries codes (who could take over).
    loop {
        let m = jo.wait("members");
        if m["list"].as_array().unwrap().iter().any(|x| x["name"] == "Sam" && x["code"] == hub_code(3)) {
            break;
        }
    }
    assert_eq!(hub.hand_over().as_deref(), Some("Sam"));
    let mv = jo.wait("moving");
    assert_eq!((mv["code"].as_str(), mv["name"].as_str()), (Some(hub_code(3).as_str()), Some("Sam")));
    // A temporary hub has nobody to hand over to.
    let t = temporary("t");
    let _a = Raw::join(t.port, hello("A", 9));
    assert!(t.hand_over().is_none());
}

#[test]
fn a_temporary_hub_keeps_no_book() {
    let hub = temporary("t");
    let mut kit = Raw::join(hub.port, hello("Kit", 1));
    assert!(kit.wait("welcome")["expiresIn"].is_null());
    kit.send(json!({ "op": "address", "code": hub_code(1) }));
    kit.send(json!({ "op": "ping" }));
    kit.wait("pong");
    assert!(hub.state.lock().unwrap().book.is_empty());
}

/// "Open to hub": what each person has open, shown to everyone, gone
/// when it closes or they leave.
#[test]
fn whats_open_is_shown_until_it_closes() {
    let hub = temporary("t");
    let mut kit = Raw::join(hub.port, hello("Kit", 1));
    kit.wait("welcome");
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    assert_eq!(jo.wait("offers")["list"], json!([]));
    kit.send(json!({ "op": "offer", "oid": "voip", "code": format!("`{VOIP}`"), "title": "Hangout" }));
    let o = jo.wait("offers");
    assert_eq!(o["list"][0]["name"], "Kit");
    assert_eq!(o["list"][0]["kind"], "voip");
    assert_eq!(o["list"][0]["code"], VOIP, "written out clean");
    // Not something a hub can share: refused.
    kit.send(json!({ "op": "offer", "oid": "x", "code": hub_code(1), "title": "x" }));
    assert_eq!(jo.wait("offers")["list"].as_array().unwrap().len(), 1);
    kit.send(json!({ "op": "withdraw", "oid": "voip" }));
    assert_eq!(jo.wait("offers")["list"], json!([]));
    // Leaving takes everything they had open with them.
    kit.send(json!({ "op": "offer", "oid": "voip", "code": VOIP, "title": "Hangout" }));
    assert_eq!(jo.wait("offers")["list"].as_array().unwrap().len(), 1);
    kit.send(json!({ "op": "bye" }));
    assert_eq!(jo.wait("offers")["list"], json!([]));
}

/// Anyone in it can delete a hub for everyone in it.
#[test]
fn delete_reaches_everyone_in_it() {
    let (hub, _) = saved("h");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    jo.send(json!({ "op": "delete" }));
    assert_eq!(kit.wait("deleted")["by"], "Jo");
    assert_eq!(jo.wait("deleted")["by"], "Jo");
}

/// Scan: a peek says what the hub is and whose copy, without joining.
#[test]
fn a_peek_answers_without_joining() {
    let (hub, id) = saved("Minecraft Hub");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    let p = peek(hub.port, Duration::from_secs(5)).unwrap();
    assert_eq!((p.id.as_str(), p.hub.as_str(), p.host.as_str(), p.members), (id.as_str(), "Minecraft Hub", "Kit", 1));
    assert_eq!(hub.count(), 1, "peeking isn't joining");
    // Nothing there: no answer.
    let nothing = crate::free_port().unwrap();
    assert!(peek(nothing, Duration::from_millis(800)).is_err());
}

#[test]
fn a_peek_asks_again_when_the_tunnel_drops_its_first_try() {
    let (hub, id) = saved("Retry Hub");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    // Stands in for TailCat: takes the first connection and closes it
    // (its first reach for the other end failed), then gets through.
    let ln = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let near = ln.local_addr().unwrap().port();
    let far = hub.port;
    std::thread::spawn(move || {
        let (first, _) = ln.accept().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(first);
        let (mut near_side, _) = ln.accept().unwrap();
        let mut far_side = TcpStream::connect((Ipv4Addr::LOCALHOST, far)).unwrap();
        let (mut a, mut b) = (near_side.try_clone().unwrap(), far_side.try_clone().unwrap());
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut a, &mut b);
        });
        let _ = std::io::copy(&mut far_side, &mut near_side);
    });
    let p = peek(near, Duration::from_secs(10)).expect("the second try answers");
    assert_eq!((p.id.as_str(), p.hub.as_str()), (id.as_str(), "Retry Hub"));
}

#[test]
fn a_peek_from_another_open_copy_reaches_only_the_host() {
    let (hub, _) = saved("h");
    let mut kit = Raw::join(hub.port, json!({ "op": "hello", "v": PROTOCOL, "peer": 1, "name": "Kit", "host": hub.host_token }));
    kit.wait("welcome");
    let mut jo = Raw::join(hub.port, hello("Jo", 2));
    jo.wait("welcome");
    // A plain scan says nothing.
    peek(hub.port, Duration::from_secs(5)).unwrap();
    // A copy that's open itself says where.
    peek_from(hub.port, Duration::from_secs(5), Some(&hub_code(7)), &AtomicBool::new(false)).unwrap();
    let also = kit.wait("alsoOpen");
    assert_eq!(also["code"].as_str(), Some(hub_code(7).as_str()));
    jo.r.get_ref().set_read_timeout(Some(Duration::from_millis(800))).unwrap();
    loop {
        let v = jo.next();
        assert_ne!(v["op"], "alsoOpen", "only the host hears");
        if v.is_null() {
            break;
        }
    }
    // Junk in place of a code: nothing.
    peek_from(hub.port, Duration::from_secs(5), Some("not a code"), &AtomicBool::new(false)).unwrap();
    kit.r.get_ref().set_read_timeout(Some(Duration::from_millis(800))).unwrap();
    loop {
        let v = kit.next();
        assert_ne!(v["op"], "alsoOpen");
        if v.is_null() {
            break;
        }
    }
}

#[test]
fn of_two_open_copies_exactly_one_moves() {
    let (a, b) = (hub_code(1), hub_code(2));
    for (na, nb) in [(1, 1), (1, 2), (2, 1), (3, 3), (5, 2), (2, 9)] {
        let a_moves = theirs_wins(&a, na, &b, nb);
        let b_moves = theirs_wins(&b, nb, &a, na);
        assert!(a_moves != b_moves, "{na} vs {nb}: exactly one moves");
        if na != nb {
            assert_eq!(a_moves, na < nb, "{na} vs {nb}: the one with fewer people moves");
        }
    }
}

#[test]
fn a_stopped_check_ends_at_once() {
    // Stands in for a TailCat that never gets through: takes the
    // connection and says nothing. Stopping closes it (as closing the
    // TailCat would), and the peek ends then, not 20 s later.
    let ln = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let near = ln.local_addr().unwrap().port();
    let stop = Arc::new(Stop::default());
    let s = stop.clone();
    std::thread::spawn(move || {
        let (conn, _) = ln.accept().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        s.stop();
        drop(conn);
    });
    let t = Instant::now();
    let r = peek_from(near, Duration::from_secs(20), None, &stop.stopped);
    assert_eq!(r.err().as_deref(), Some("stopped"));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn stopping_closes_the_checks_tailcats_and_no_more_start() {
    let stop = Stop::default();
    let slot = stop.hold(Proc::stand_in()).expect("held while not stopped");
    assert!(slot.lock().unwrap().is_some());
    stop.stop();
    assert!(slot.lock().unwrap().is_none(), "its TailCat is closed");
    assert!(stop.is_stopped());
    // One that starts after: closed straight away, not held.
    assert!(stop.hold(Proc::stand_in()).is_none());
}

#[test]
fn looks_come_soon_then_every_minute() {
    let secs: Vec<u64> = (0..9).map(|n| look_after(n).as_secs()).collect();
    assert_eq!(secs, [10, 20, 40, 60, 60, 60, 60, 60, 60]);
    // Missing a few pings isn't gone; a restarted PC is, soon.
    assert!(SILENT_FOR >= PING_EVERY * 4 && SILENT_FOR > Duration::from_secs(15) + Duration::from_secs(5));
    assert!(SILENT_FOR + TAKE_OVER_AFTER <= Duration::from_secs(45));
}

#[test]
fn a_saved_hub_needs_its_own_port() {
    let (hub, _) = saved("h");
    let again = HubServer::start(Meta { id: hubstore::new_id(), name: "x".into(), expires: Some(hubstore::now() + 60) }, Some(hub.port));
    assert!(again.err().unwrap().contains("Another program"));
}

#[test]
fn invite_kinds() {
    assert_eq!(invite_kind(VOIP), Some("voip"));
    let addr = VOIP.rsplit(':').next().unwrap();
    assert_eq!(invite_kind(&format!("kitty:port:25565:{addr}")), Some("port"));
    assert_eq!(invite_kind(&format!("kitty:game.minecraft:25565:{addr}")), Some("game"));
    assert_eq!(invite_kind(addr), None, "a bare address needs port numbers told separately");
    assert_eq!(invite_kind(&format!("kitty:hub:5:{addr}")), None);
    assert_eq!(clean("  a\u{7}b  ", 10), "ab");
    assert!(same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "ab"));
}
