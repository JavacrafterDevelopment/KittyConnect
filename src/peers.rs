//! Who is connected to something you opened, by name, and how.
//!
//! Everyone who joins a port or a file share sends a check through the tunnel
//! every 30 seconds (and again at once when their path or name changes):
//!
//!   → {"op":"ping","peer":<random id>,"name":"Sam","path":"direct","rtt":23,"addr":"tc…"}
//!   ← {"pong":true,"name":"<host's name>"}
//!
//! and `{"op":"bye","peer":…}` when they disconnect. Only the joiner can tell
//! whether its own tunnel went direct (TailCat logs it on that side), so each
//! one reports it, the same way room members do. The host lists everyone it
//! has heard from recently. Names only ever travel inside the tunnel.
//!
//! Older versions send a bare `{"op":"ping"}` and get `{"pong":true}`; they
//! still work, they just aren't listed.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

/// The longest name anyone sees, the same as in rooms.
pub const MAX_NAME: usize = 32;

/// Not heard from in this long (two missed checks and some): no longer listed.
pub const GONE_AFTER: Duration = Duration::from_secs(75);

/// Printable, trimmed, length-capped.
pub fn clean_name(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    s.trim().chars().take(MAX_NAME).collect::<String>().trim().to_string()
}

static MY_NAME: Mutex<String> = Mutex::new(String::new());

/// The name picked at startup, used everywhere in the app.
pub fn set_my_name(name: &str) {
    *MY_NAME.lock().unwrap() = clean_name(name);
}

pub fn my_name() -> String {
    MY_NAME.lock().unwrap().clone()
}

/// A random id for one connection, so the host can tell joiners apart (two
/// people can pick the same name).
pub fn new_id() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish().max(1)
}

/// What a joiner says about itself in each check.
pub struct Report<'a> {
    pub peer: u64,
    /// The code it joined with: a port host can have several open, all behind
    /// the one beacon, and this says which.
    pub addr: &'a str,
    pub path: &'static str,
    pub rtt: Option<u32>,
}

impl Report<'_> {
    pub fn ping(&self) -> String {
        json!({ "op": "ping", "peer": self.peer, "name": my_name(), "path": self.path, "rtt": self.rtt, "addr": self.addr })
            .to_string()
    }

    pub fn bye(&self) -> String {
        json!({ "op": "bye", "peer": self.peer, "addr": self.addr }).to_string()
    }
}

/// The host's answer to a check.
pub fn pong() -> String {
    format!("{}\n", json!({ "pong": true, "name": my_name() }))
}

/// A check or goodbye as the host receives it.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Heard {
    pub op: String,
    pub peer: u64,
    pub name: String,
    pub path: String,
    pub rtt: Option<u32>,
    pub addr: String,
}

impl Heard {
    pub fn parse(line: &str) -> Option<Heard> {
        serde_json::from_str(line.trim()).ok()
    }

    /// From a version that says who it is (not a bare ping).
    pub fn is_named(&self) -> bool {
        self.peer != 0 && (self.op == "ping" || self.op == "bye")
    }
}

struct Peer {
    id: u64,
    name: String,
    path: &'static str,
    rtt: Option<u32>,
    seen: Instant,
}

/// Everyone connected to one thing this PC opened, in the order they came.
#[derive(Default)]
pub struct Peers(Vec<Peer>);

impl Peers {
    /// Takes in a check or goodbye. Returns true if the list changed.
    pub fn heard(&mut self, h: &Heard) -> bool {
        if h.op == "bye" {
            let before = self.0.len();
            self.0.retain(|p| p.id != h.peer);
            return self.0.len() != before;
        }
        let path = match h.path.as_str() {
            "direct" => "direct",
            "relay" => "relay",
            _ => "unknown",
        };
        let mut name = clean_name(&h.name);
        if name.is_empty() {
            name = "Someone".into();
        }
        let now = Instant::now();
        if let Some(p) = self.0.iter_mut().find(|p| p.id == h.peer) {
            let changed = p.name != name || p.path != path || p.rtt != h.rtt;
            p.name = name;
            p.path = path;
            p.rtt = h.rtt;
            p.seen = now;
            return changed;
        }
        self.0.push(Peer { id: h.peer, name, path, rtt: h.rtt, seen: now });
        true
    }

    /// Drops anyone not heard from in a while. Returns true if anyone went.
    pub fn expire(&mut self) -> bool {
        let before = self.0.len();
        self.0.retain(|p| p.seen.elapsed() < GONE_AFTER);
        self.0.len() != before
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn json(&self) -> Value {
        json!(self
            .0
            .iter()
            .map(|p| json!({
                "id": p.id.to_string(),
                "name": p.name,
                "path": p.path,
                "rtt": p.rtt,
                "checkedAgo": p.seen.elapsed().as_secs(),
            }))
            .collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(peer: u64, name: &str, path: &str) -> Heard {
        Heard::parse(&json!({ "op": "ping", "peer": peer, "name": name, "path": path, "rtt": 20, "addr": "tcx" }).to_string())
            .unwrap()
    }

    #[test]
    fn lists_each_joiner_with_their_own_path() {
        let mut peers = Peers::default();
        assert!(peers.heard(&ping(1, "Alex", "direct")));
        assert!(peers.heard(&ping(2, "Jo", "relay")));
        // Same name, different person: listed twice.
        assert!(peers.heard(&ping(3, "Jo", "weird")));
        // A repeat with nothing new changes nothing.
        assert!(!peers.heard(&ping(1, "Alex", "direct")));
        let list = peers.json();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["path"], "direct");
        assert_eq!(list[1]["path"], "relay");
        assert_eq!(list[2]["path"], "unknown");

        assert!(peers.heard(&ping(2, "Jo", "direct")));
        assert_eq!(peers.json()[1]["path"], "direct");

        let bye = Heard::parse(r#"{"op":"bye","peer":2}"#).unwrap();
        assert!(bye.is_named() && peers.heard(&bye));
        assert_eq!(peers.json().as_array().unwrap().len(), 2);
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Sam\u{7}\n "), "Sam");
        assert_eq!(clean_name(&"x".repeat(50)).len(), MAX_NAME);
        let mut peers = Peers::default();
        peers.heard(&ping(9, "   ", "relay"));
        assert_eq!(peers.json()[0]["name"], "Someone");
    }

    #[test]
    fn old_pings_are_not_listed() {
        let h = Heard::parse(r#"{"op":"ping"}"#).unwrap();
        assert!(!h.is_named());
        assert!(Heard::parse("not json").is_none());
    }

    #[test]
    fn report_round_trips() {
        set_my_name(" Kit ");
        let r = Report { peer: 42, addr: "tcabc", path: "relay", rtt: Some(31) };
        let h = Heard::parse(&r.ping()).unwrap();
        assert!(h.is_named());
        assert_eq!((h.peer, h.name.as_str(), h.path.as_str(), h.rtt, h.addr.as_str()), (42, "Kit", "relay", Some(31), "tcabc"));
        assert!(pong().contains("\"Kit\""));
    }
}
