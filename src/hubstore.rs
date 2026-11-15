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
//! The address book is versioned: every entry (someone saved it, or changed
//! their name) and every deletion carries when it happened, and copies merge
//! code by code, newest wins. So whatever order people come back in, after
//! weeks away or not, everyone ends up with the same book: an old copy can't
//! bring back someone deleted or an old name, and saving again after a
//! delete brings them back.
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
    /// When they saved it, or last changed their name (seconds since 1970;
    /// 0 from KittyConnect 0.12.0, which didn't say).
    #[serde(default)]
    pub at: u64,
}

/// A code deleted from the hub, and when.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gone {
    pub code: String,
    #[serde(default)]
    pub at: u64,
}

/// Reads deleted codes saved by 0.12.0 (plain codes) or later (with times).
fn gone_any<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Gone>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Any {
        Code(String),
        Full(Gone),
    }
    let v: Vec<Any> = Vec::deserialize(d)?;
    Ok(v.into_iter().map(|a| match a {
        Any::Code(code) => Gone { code, at: 0 },
        Any::Full(g) => g,
    }).collect())
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
    #[serde(default, deserialize_with = "gone_any")]
    pub gone: Vec<Gone>,
    /// When the address book last changed (0: not since it was saved).
    #[serde(default)]
    pub updated: u64,
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

/// An address book entry as it arrives: a hub code (written out clean), a
/// name and when. None if it isn't one.
pub fn entry(name: &str, code_text: &str, at: u64) -> Option<Entry> {
    let mut name = crate::hub::clean(name, crate::peers::MAX_NAME);
    if name.is_empty() {
        name = "Someone".into();
    }
    Some(Entry { name, code: clean_code(code_text)?, at: when(at) })
}

/// A hub code written out clean. None if it isn't one.
pub fn clean_code(code_text: &str) -> Option<String> {
    let c = code::parse(code_text).ok().filter(|c| c.kind == Kind::Hub)?;
    code::canonical(&c)
}

/// A time from someone else's copy: never more than a day ahead of ours, so a
/// wrong clock can't make its change win for good.
fn when(at: u64) -> u64 {
    at.min(now() + 24 * 3600)
}

/// Merges another copy's book into ours, code by code, newest wins: an entry
/// (someone saved it, or renamed) against our entry for that code, or
/// against a deletion. A deletion wins a tie. Keeps the book within
/// [`MAX_BOOK`] and the deletions within [`MAX_GONE`]. True if anything changed.
pub fn merge(book: &mut Vec<Entry>, gone: &mut Vec<Gone>, entries: &[Entry], deletes: &[Gone]) -> bool {
    let (book_before, gone_before) = (book.clone(), gone.clone());
    for d in deletes {
        let Some(code) = clean_code(&d.code) else { continue };
        let at = when(d.at);
        match gone.iter_mut().find(|g| g.code == code) {
            Some(g) => g.at = g.at.max(at),
            None => gone.push(Gone { code: code.clone(), at }),
        }
        let at = gone.iter().find(|g| g.code == code).map_or(at, |g| g.at);
        book.retain(|e| !(e.code == code && e.at <= at));
    }
    for e in entries {
        let Some(e) = entry(&e.name, &e.code, e.at) else { continue };
        if let Some(g) = gone.iter().find(|g| g.code == e.code) {
            if g.at >= e.at {
                continue;
            }
            // Saved again after it was deleted.
            gone.retain(|g| g.code != e.code);
        }
        if let Some(b) = book.iter_mut().find(|b| b.code == e.code) {
            if e.at > b.at {
                *b = e;
            }
        } else if book.len() < MAX_BOOK {
            book.push(e);
        }
    }
    if gone.len() > MAX_GONE {
        gone.sort_by_key(|g| std::cmp::Reverse(g.at));
        gone.truncate(MAX_GONE);
    }
    *book != book_before || *gone != gone_before
}

/// Deleted codes as 0.12.0 reads them (plain codes, no times).
pub fn gone_codes(gone: &[Gone]) -> Vec<&str> {
    gone.iter().map(|g| g.code.as_str()).collect()
}

/// Deleted codes from a message: `goneAt` (with times) if it has it, else
/// `gone` (0.12.0: plain codes).
pub fn gone_from(msg: &serde_json::Value) -> Vec<Gone> {
    if let Some(list) = msg["goneAt"].as_array() {
        return list.iter().filter_map(|v| serde_json::from_value::<Gone>(v.clone()).ok()).collect();
    }
    msg["gone"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).map(|c| Gone { code: c.to_string(), at: 0 }).collect()).unwrap_or_default()
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

    fn e(name: &str, code: &str, at: u64) -> Entry {
        Entry { name: name.into(), code: code.into(), at }
    }

    fn g(code: &str, at: u64) -> Gone {
        Gone { code: code.into(), at }
    }

    #[test]
    fn the_address_book_merges_and_remembers_deletions() {
        let a = code::hub(21001, A);
        let b = code::hub(21002, B);
        let (mut book, mut gone) = (Vec::new(), Vec::new());
        assert!(merge(&mut book, &mut gone, &[e("Kit", &format!("`{a}`"), 100)], &[]));
        assert_eq!(book, vec![e("Kit", &a, 100)]);
        // Not a hub code: left out.
        assert!(!merge(&mut book, &mut gone, &[e("x", &code::voip(1, A), 100)], &[]));
        // A newer name for the same code replaces it.
        assert!(merge(&mut book, &mut gone, &[e("Kitty", &a, 200)], &[]));
        assert_eq!(book.len(), 1);
        // Deleted codes go, and an old copy can't bring them back.
        assert!(merge(&mut book, &mut gone, &[e("Jo", &b, 300)], &[g(&a, 250)]));
        assert_eq!(book, vec![e("Jo", &b, 300)]);
        assert!(!merge(&mut book, &mut gone, &[e("Kitty", &a, 200)], &[]));
    }

    /// Away for two weeks with an old book: rejoining brings the new people
    /// in, and the old book changes nothing that's newer.
    #[test]
    fn an_old_copy_catches_up_and_cant_undo_anything() {
        let (kit, jo, sam, zed) = (code::hub(21001, A), code::hub(21002, B), code::hub(21003, A), code::hub(21004, B));
        // The hub's book now: Jo renamed, Sam joined, Zed joined and later left.
        let mut hub = vec![e("Kit", &kit, 10), e("Jo (new name)", &jo, 500), e("Sam", &sam, 600)];
        let mut hub_gone = vec![g(&zed, 700)];
        // Ours, from two weeks ago: Jo's old name, Zed still in it.
        let (mut mine, mut mine_gone) = (vec![e("Kit", &kit, 10), e("Jo", &jo, 20), e("Zed", &zed, 30)], vec![]);
        // We come back: we merge theirs, they merge ours.
        assert!(merge(&mut mine, &mut mine_gone, &hub, &hub_gone));
        merge(&mut hub, &mut hub_gone, &mine.clone(), &mine_gone.clone());
        for b in [&mine, &hub] {
            let names: Vec<&str> = b.iter().map(|x| x.name.as_str()).collect();
            assert_eq!(names, ["Kit", "Jo (new name)", "Sam"], "new people in, new name kept, Zed stays out");
        }
        // Zed saves it again later: they're back, for everyone.
        assert!(merge(&mut mine, &mut mine_gone, &[e("Zed", &zed, 800)], &[]));
        assert!(mine.iter().any(|x| x.code == zed));
        assert!(mine_gone.is_empty());
        // A clock far in the future can't win for good.
        merge(&mut mine, &mut mine_gone, &[e("Sam (from the future)", &sam, u64::MAX)], &[]);
        assert!(merge(&mut mine, &mut mine_gone, &[], &[g(&sam, now() + 2 * 24 * 3600)]), "a later delete still wins");
    }

    #[test]
    fn old_saved_files_still_load() {
        let old = r#"{"id":"00000000000000000000000000000000","name":"x","expires":1,"port":21000,"code":"c","book":[{"name":"Kit","code":"c"}],"gone":["d"]}"#;
        let s: Saved = serde_json::from_str(old).unwrap();
        assert_eq!((s.book[0].at, s.gone[0].clone()), (0, g("d", 0)));
        let msg = serde_json::json!({ "gone": ["d"] });
        assert_eq!(gone_from(&msg), vec![g("d", 0)]);
        let msg = serde_json::json!({ "gone": ["d"], "goneAt": [{ "code": "d", "at": 5 }] });
        assert_eq!(gone_from(&msg), vec![g("d", 5)]);
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
            updated: 0,
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
