//! Who is connected to something you opened, by name, and how.
//!
//! Everyone who joins a port or a file share sends a check through the tunnel
//! every 30 seconds (and again at once when their path or name changes):
//!
//!   → {"op":"ping","peer":<random id>,"name":"Sam","path":"direct","rtt":23,"addr":"tc…"}
//!   ← {"pong":true,"name":"<host's name>"}             (+ "retry":true, see below)
//!
//! and `{"op":"bye","peer":…}` when they disconnect. Only the joiner can tell
//! whether its own tunnel went direct (TailCat logs it on that side), so each
//! one reports it, the same way room members do. The host lists everyone it
//! has heard from recently. Names only ever travel inside the tunnel.
//!
//! The host can press "Retry direct" next to someone on a relay: their next
//! check (every 10 seconds while on a relay) gets `"retry":true`, and they
//! restart their end of the tunnel, which tries for a direct path from scratch.
//!
//! Port check: until it has passed, a joiner's check also carries the port
//! numbers it typed (`"ports":[25565]`). The host answers `"ports":"open"`
//! only if that code opened every one of them, else `"ports":"wrong"`, and
//! the joiner stops: nothing on its PC stays listening for a port nobody
//! opened. It answers for the ports asked and nothing else, so the code
//! still never tells anyone which ports it opened, and it answers a few times
//! a minute at most ("busy" after that), so nobody can guess through them.
//!
//! Older versions send a bare `{"op":"ping"}` and get `{"pong":true}`; they
//! still work, they just aren't listed.

use std::collections::VecDeque;
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
    let mut b = [0u8; 8];
    fill_random(&mut b);
    u64::from_le_bytes(b).max(1)
}

/// A secret: 32 bytes from the OS's secure random source, as hex.
pub fn secret() -> String {
    let mut b = [0u8; 32];
    fill_random(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Fills `buf` from the OS's secure random source. Should that ever fail
/// (it doesn't on Windows or macOS), a keyed hash seeded by the OS is the
/// fallback, so nothing is ever left predictable.
fn fill_random(buf: &mut [u8]) {
    if getrandom::fill(buf).is_ok() {
        return;
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
}

/// What a joiner says about itself in each check.
pub struct Report<'a> {
    pub peer: u64,
    /// The code it joined with: a port host can have several open, all behind
    /// the one beacon, and this says which.
    pub addr: &'a str,
    pub path: &'static str,
    pub rtt: Option<u32>,
    /// The port numbers to check, until the host has said they're open.
    pub ports: Vec<u16>,
}

impl Report<'_> {
    pub fn ping(&self) -> String {
        let mut v = json!({ "op": "ping", "peer": self.peer, "name": my_name(), "path": self.path, "rtt": self.rtt, "addr": self.addr });
        if !self.ports.is_empty() {
            v["ports"] = json!(self.ports);
        }
        v.to_string()
    }

    pub fn bye(&self) -> String {
        json!({ "op": "bye", "peer": self.peer, "addr": self.addr }).to_string()
    }
}

static RETRY: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The host asks `peer` to reconnect, at its next check.
pub fn ask_retry(peer: u64) {
    let mut r = RETRY.lock().unwrap();
    if !r.contains(&peer) {
        r.push(peer);
    }
}

/// The host's answer to a check from `peer` (0 for an older version that
/// doesn't say who it is), with the port check's answer if it asked one.
pub fn pong(peer: u64, ports: Option<PortCheck>) -> String {
    let retry = {
        let mut r = RETRY.lock().unwrap();
        let asked = peer != 0 && r.contains(&peer);
        r.retain(|p| *p != peer);
        asked
    };
    let mut v = json!({ "pong": true, "name": my_name() });
    if retry {
        v["retry"] = json!(true);
    }
    if let Some(p) = ports {
        v["ports"] = json!(p.as_str());
    }
    format!("{v}\n")
}

// ---------------------------------------------------------------- port check

/// The most port numbers one check may ask about.
pub const MAX_CHECKED: usize = 16;
/// Port checks answered per code in [`CHECK_WINDOW`]; more get "busy".
const CHECKS_PER_WINDOW: usize = 10;
const CHECK_WINDOW: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortCheck {
    /// The code opened every port asked about.
    Open,
    /// It didn't (or there is no such code here).
    Wrong,
    /// Asked too often just now; ask again in a moment.
    Busy,
}

impl PortCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            PortCheck::Open => "open",
            PortCheck::Wrong => "wrong",
            PortCheck::Busy => "busy",
        }
    }

    pub fn parse(s: &str) -> Option<PortCheck> {
        match s {
            "open" => Some(PortCheck::Open),
            "wrong" => Some(PortCheck::Wrong),
            "busy" => Some(PortCheck::Busy),
            _ => None,
        }
    }
}

struct Served {
    addr: String,
    ports: Vec<u16>,
    checks: VecDeque<Instant>,
}

/// Every code this PC serves behind the beacon, with the ports it opened.
static SERVED: Mutex<Vec<Served>> = Mutex::new(Vec::new());

/// A code is open (TailCat printed its address): its ports can be checked.
pub fn serve(addr: &str, ports: &[u16]) {
    let mut s = SERVED.lock().unwrap();
    s.retain(|e| e.addr != addr);
    s.push(Served { addr: addr.to_string(), ports: ports.to_vec(), checks: VecDeque::new() });
}

/// The code closed: every check on it is "wrong" from now on.
pub fn unserve(addr: &str) {
    SERVED.lock().unwrap().retain(|e| e.addr != addr);
}

/// Whether the code `addr` opened every one of `asked`. None if nothing was
/// asked (an older joiner, or one whose check already passed).
pub fn check_ports(addr: &str, asked: &[u16]) -> Option<PortCheck> {
    if asked.is_empty() {
        return None;
    }
    let mut served = SERVED.lock().unwrap();
    let Some(e) = served.iter_mut().find(|e| !addr.is_empty() && e.addr == addr) else {
        return Some(PortCheck::Wrong);
    };
    let now = Instant::now();
    while e.checks.front().is_some_and(|t| now.duration_since(*t) > CHECK_WINDOW) {
        e.checks.pop_front();
    }
    if e.checks.len() >= CHECKS_PER_WINDOW {
        return Some(PortCheck::Busy);
    }
    e.checks.push_back(now);
    let open = asked.len() <= MAX_CHECKED && asked.iter().all(|p| e.ports.contains(p));
    Some(if open { PortCheck::Open } else { PortCheck::Wrong })
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
    pub ports: Vec<u16>,
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
    /// When the host last pressed "Retry direct" for them.
    retry_asked: Option<Instant>,
}

/// How long "Retrying…" shows after pressing Retry direct.
const RETRY_SHOWN: Duration = Duration::from_secs(45);

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
            if path == "direct" {
                p.retry_asked = None;
            }
            p.name = name;
            p.path = path;
            p.rtt = h.rtt;
            p.seen = now;
            return changed;
        }
        self.0.push(Peer { id: h.peer, name, path, rtt: h.rtt, seen: now, retry_asked: None });
        true
    }

    /// "Retry direct" for one person. Returns true if they're on this list.
    pub fn retry(&mut self, id: u64) -> bool {
        let Some(p) = self.0.iter_mut().find(|p| p.id == id) else { return false };
        p.retry_asked = Some(Instant::now());
        ask_retry(id);
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
                "retrying": p.retry_asked.is_some_and(|t| t.elapsed() < RETRY_SHOWN),
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
        let r = Report { peer: 42, addr: "tcabc", path: "relay", rtt: Some(31), ports: vec![25565] };
        let h = Heard::parse(&r.ping()).unwrap();
        assert!(h.is_named());
        assert_eq!((h.peer, h.name.as_str(), h.path.as_str(), h.rtt, h.addr.as_str()), (42, "Kit", "relay", Some(31), "tcabc"));
        assert_eq!(h.ports, vec![25565]);
        assert!(pong(42, None).contains("\"Kit\""));
        // Once the check has passed, the ports aren't sent again.
        let r = Report { ports: Vec::new(), ..r };
        assert!(!r.ping().contains("ports"));
    }

    #[test]
    fn retry_reaches_only_that_person_once() {
        let mut peers = Peers::default();
        peers.heard(&ping(5, "Jo", "relay"));
        assert!(peers.retry(5));
        assert!(!peers.retry(6));
        assert_eq!(peers.json()[0]["retrying"], true);
        assert!(!pong(7, None).contains("retry"));
        assert!(!pong(0, None).contains("retry"));
        assert!(pong(5, None).contains("\"retry\":true"));
        assert!(!pong(5, None).contains("retry"));
        // Going direct clears "Retrying…".
        peers.heard(&ping(5, "Jo", "direct"));
        assert_eq!(peers.json()[0]["retrying"], false);
    }

    #[test]
    fn a_port_the_code_did_not_open_is_wrong() {
        serve("tcports1", &[25565, 47474]);
        assert_eq!(check_ports("tcports1", &[25565]), Some(PortCheck::Open));
        assert_eq!(check_ports("tcports1", &[25566]), Some(PortCheck::Wrong));
        // All or nothing: which one is wrong isn't said.
        assert_eq!(check_ports("tcports1", &[25565, 25566]), Some(PortCheck::Wrong));
        assert_eq!(check_ports("tcports1", &[]), None);
        // Another code's ports don't count, nor a code that isn't open here.
        serve("tcports2", &[7777]);
        assert_eq!(check_ports("tcports2", &[25565]), Some(PortCheck::Wrong));
        assert_eq!(check_ports("tcnothere", &[25565]), Some(PortCheck::Wrong));
        assert_eq!(check_ports("", &[25565]), Some(PortCheck::Wrong));
        unserve("tcports1");
        assert_eq!(check_ports("tcports1", &[25565]), Some(PortCheck::Wrong));
        unserve("tcports2");
        assert!(pong(0, Some(PortCheck::Wrong)).contains("\"ports\":\"wrong\""));
    }

    #[test]
    fn port_checks_cannot_be_used_to_guess() {
        serve("tcports3", &[40000]);
        let mut answered = 0;
        for p in 1..=50u16 {
            match check_ports("tcports3", &[p]) {
                Some(PortCheck::Busy) => {}
                _ => answered += 1,
            }
        }
        assert_eq!(answered, CHECKS_PER_WINDOW);
        let many: Vec<u16> = (39990..40010).collect();
        unserve("tcports3");
        serve("tcports3", &(39990..40010).collect::<Vec<_>>());
        // More than MAX_CHECKED at once is never "open".
        assert_eq!(check_ports("tcports3", &many), Some(PortCheck::Wrong));
        unserve("tcports3");
    }

    #[test]
    fn secrets_are_long_and_never_repeat() {
        let a = secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, secret());
        assert_ne!(new_id(), new_id());
    }
}
