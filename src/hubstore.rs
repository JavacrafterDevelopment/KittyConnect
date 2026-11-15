//! Saved hubs: a hub kept for up to 13 weeks, so it can be opened again
//! with the same code, by anyone in it who saved it.
//!
//! Each person who saves a hub has their own TailCat key for it, made with
//! the official `tailcat genkey --fixed-region` (with TailCat's WireGuard
//! pre-shared key, as always), so their code is the same every time they
//! open it; and a fixed local port, which the code names. What everyone's
//! copies share is the hub's id, name and expiry, and its address book:
//! each person's hub code, so anyone can scan for whoever has it open.
//! What's shared *in* a hub (rooms, files, ports, folders) still gets new
//! keys every time.
//!
//! On disk, in the data folder: `hubs/<id>/hub.json` and
//! `hubs/<id>/key.private.json` (TailCat's own key file). An expired hub is
//! deleted, key and all.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::code::{self, Kind};

/// The longest a hub can be kept: 13 weeks.
pub const MAX_LIFE: u64 = 13 * 7 * 24 * 3600;
/// And the shortest: an hour.
pub const MIN_LIFE: u64 = 3600;
/// The most people an address book remembers.
pub const MAX_BOOK: usize = 64;
/// Codes deleted from a hub, remembered so an old copy can't bring them back.
pub const MAX_GONE: usize = 128;

/// One person's code for the hub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub code: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Saved {
    /// The hub's id, the same in everyone's copy (32 hex digits).
    pub id: String,
    pub name: String,
    /// When it's deleted, in seconds since 1970.
    pub expires: u64,
    /// The local port our copy's server listens on (our code names it).
    pub port: u16,
    /// Our own code for it.
    pub code: String,
    #[serde(default)]
    pub book: Vec<Entry>,
    #[serde(default)]
    pub gone: Vec<String>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A hub id: exactly 32 lowercase hex digits (it names a folder, and comes
/// over the network).
pub fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn new_id() -> String {
    crate::peers::secret()[..32].to_string()
}

/// How long to keep a hub, kept within an hour and 13 weeks.
pub fn clamp_life(secs: u64) -> u64 {
    secs.clamp(MIN_LIFE, MAX_LIFE)
}

/// Where saved hubs live: `hubs/` in the data folder.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Default for Store {
    fn default() -> Store {
        Store { root: crate::portable::data_dir().join("hubs") }
    }
}

impl Store {
    /// Somewhere else (the end-to-end check runs two apps on one PC).
    #[cfg(test)]
    pub fn at(root: PathBuf) -> Store {
        Store { root }
    }

    fn dir(&self, id: &str) -> Option<PathBuf> {
        is_id(id).then(|| self.root.join(id))
    }

    /// Where TailCat keeps our key for this hub.
    pub fn key_path(&self, id: &str) -> Option<PathBuf> {
        self.dir(id).map(|d| d.join("key.private.json"))
    }

    /// Every saved hub that hasn't expired, soonest to expire first. Expired
    /// or broken ones are deleted.
    pub fn load_all(&self) -> Vec<Saved> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&self.root) else { return out };
    let now = now();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !is_id(&name) {
            continue;
        }
        let saved = std::fs::read(e.path().join("hub.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
            .filter(|s| s.id == name && s.expires > now && self.key_path(&s.id).is_some_and(|k| k.is_file()));
        match saved {
            Some(s) => out.push(s),
            None => self.delete(&name),
        }
    }
    out.sort_by_key(|s| s.expires);
    out
    }

    pub fn store(&self, s: &Saved) -> Result<(), String> {
    let d = self.dir(&s.id).ok_or("Bad hub id.")?;
    std::fs::create_dir_all(&d).map_err(|e| format!("Couldn't save the hub: {e}"))?;
    let tmp = d.join("hub.json.part");
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| format!("Couldn't save the hub: {e}"))?;
    std::fs::rename(&tmp, d.join("hub.json")).map_err(|e| format!("Couldn't save the hub: {e}"))
    }

    /// Deletes a saved hub, key and all.
    pub fn delete(&self, id: &str) {
        if let Some(d) = self.dir(id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Makes the folder for a new saved hub, so TailCat can write its key there.
    pub fn prepare(&self, id: &str) -> Result<PathBuf, String> {
        let d = self.dir(id).ok_or("Bad hub id.")?;
        std::fs::create_dir_all(&d).map_err(|e| format!("Couldn't save the hub: {e}"))?;
        self.key_path(id).ok_or_else(|| "Bad hub id.".into())
    }
}

/// An address book entry as it arrives: a hub code (written out clean) and
/// a name. None if it isn't one.
pub fn entry(name: &str, code_text: &str) -> Option<Entry> {
    let c = code::parse(code_text).ok().filter(|c| c.kind == Kind::Hub)?;
    let mut name = crate::hub::clean(name, crate::peers::MAX_NAME);
    if name.is_empty() {
        name = "Someone".into();
    }
    Some(Entry { name, code: code::canonical(&c)? })
}

/// Adds `incoming` to `book` (newest name wins for a code), leaving out
/// anything in `gone`, and keeps it within [`MAX_BOOK`]. True if it changed.
pub fn merge(book: &mut Vec<Entry>, gone: &[String], incoming: &[Entry]) -> bool {
    let before = book.clone();
    book.retain(|e| !gone.contains(&e.code));
    for e in incoming {
        let Some(e) = entry(&e.name, &e.code) else { continue };
        if gone.contains(&e.code) {
            continue;
        }
        if let Some(b) = book.iter_mut().find(|b| b.code == e.code) {
            b.name = e.name;
        } else if book.len() < MAX_BOOK {
            book.push(e);
        }
    }
    *book != before
}

/// Adds codes to the deleted list (cleaned, within [`MAX_GONE`]). True if it changed.
pub fn add_gone(gone: &mut Vec<String>, codes: &[String]) -> bool {
    let mut changed = false;
    for c in codes {
        let Some(e) = entry("", c) else { continue };
        if !gone.contains(&e.code) {
            gone.push(e.code);
            changed = true;
        }
    }
    if gone.len() > MAX_GONE {
        let extra = gone.len() - MAX_GONE;
        gone.drain(..extra);
    }
    changed
}

/// A local port for a new saved hub, kept for its whole life (its code
/// names it). Out of the range systems hand out for outgoing connections,
/// and free on both 127.0.0.1 and ::1 right now.
pub fn pick_port() -> Result<u16, String> {
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
    for _ in 0..200 {
        let mut b = [0u8; 2];
        let _ = getrandom::fill(&mut b);
        let port = 21000 + (u16::from_le_bytes(b) % 11000);
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port));
        let v6_free = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::AddrInUse,
        };
        if v4.is_ok() && v6_free {
            return Ok(port);
        }
    }
    Err("Couldn't find a free port for the hub.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN";
    const B: &str = "tcpGFwWCDOabwiILJFk5qR7vW3yZ8aB4cD6eF1gH5iJ0kLmNomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdn";

    #[test]
    fn ids_and_lives() {
        assert!(is_id(&new_id()));
        assert!(!is_id("../../etc"));
        assert!(!is_id(&"A".repeat(32)));
        assert!(!is_id(&"a".repeat(31)));
        assert_eq!(clamp_life(10), MIN_LIFE);
        assert_eq!(clamp_life(MAX_LIFE * 4), MAX_LIFE);
        assert_eq!(clamp_life(3 * 24 * 3600), 3 * 24 * 3600);
        assert!(Store::default().key_path("../x").is_none());
    }

    #[test]
    fn the_address_book_merges_and_remembers_deletions() {
        let a = code::hub(21001, A);
        let b = code::hub(21002, B);
        let mut book = Vec::new();
        assert!(merge(&mut book, &[], &[Entry { name: "Kit".into(), code: format!("`{a}`") }]));
        assert_eq!(book, vec![Entry { name: "Kit".into(), code: a.clone() }]);
        // Not a hub code: left out.
        assert!(!merge(&mut book, &[], &[Entry { name: "x".into(), code: code::voip(1, A) }]));
        // A new name for the same code replaces it.
        assert!(merge(&mut book, &[], &[Entry { name: "Kitty".into(), code: a.clone() }]));
        assert_eq!(book.len(), 1);
        // Deleted codes go, and an old copy can't bring them back.
        let mut gone = Vec::new();
        assert!(add_gone(&mut gone, &[a.clone()]));
        assert!(merge(&mut book, &gone, &[Entry { name: "Jo".into(), code: b.clone() }]));
        assert_eq!(book, vec![Entry { name: "Jo".into(), code: b.clone() }]);
        assert!(!merge(&mut book, &gone, &[Entry { name: "Kit".into(), code: a }]));
    }

    #[test]
    fn saved_hubs_round_trip_and_expire() {
        let st = Store::at(std::env::temp_dir().join(format!("kc-hubs-{}", new_id())));
        let mut s = Saved {
            id: new_id(),
            name: "Minecraft Hub".into(),
            expires: now() + 3600,
            port: pick_port().unwrap(),
            code: code::hub(21001, A),
            book: Vec::new(),
            gone: Vec::new(),
        };
        let key = st.prepare(&s.id).unwrap();
        std::fs::write(&key, "{}").unwrap();
        st.store(&s).unwrap();
        assert!(st.load_all().iter().any(|x| x.id == s.id && x.name == "Minecraft Hub"));
        s.expires = now() - 1;
        st.store(&s).unwrap();
        assert!(!st.load_all().iter().any(|x| x.id == s.id), "an expired hub is gone");
        assert!(!key.exists(), "key and all");
    }
}
