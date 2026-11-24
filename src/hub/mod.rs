//! The Hub: one code for a group of friends.
//!
//! Whoever hosts the hub runs a small server on this PC (127.0.0.1 and ::1
//! only) and serves its one port through the official TailCat, with a new key
//! and pre-shared key each time: `tailcat serve --key=new <port>`. Friends join
//! with the hub code, `kitty:hub:<port>:<address>`, through the official
//! TailCat too (`tailcat forward`), and each keeps one connection to the hub
//! inside that encrypted tunnel. The host's own KittyConnect connects to its
//! server directly, with a secret only it knows, which makes it the host.
//!
//! Over that connection everyone sees who is in the hub (and whether each is
//! direct or on a relay), and anyone can invite others to something they
//! opened elsewhere in the app: a voice room, a file share, ports, a game
//! server or a shared folder. The invite carries that thing's code, so nobody
//! copies and pastes it; the room or folder itself still connects the way it
//! always does (a voice room through the VOIP helper, the rest through the
//! official TailCat). Nothing is joined until the person invited presses Join.
//!
//! A hub is temporary (a new key every time, like everything else), or
//! saved for up to 13 weeks (see [`crate::hubstore`]): then each person who
//! saves it opens it again with their own same code, and everyone's copies
//! share an address book, so anyone can scan for whoever has it open.
//!
//! One JSON object per line, each way:
//!
//!   → {"op":"hello","v":2,"peer":<random>,"name":"Sam","path":"direct","rtt":20[,"host":<secret>]}
//!   ← {"op":"welcome","v":2,"you":3,"hub":"Minecraft Hub","id":"<32 hex>","expiresIn":<secs>|null,"host":false}
//!     (or {"op":"full"} / {"op":"old"}), then the members, address book and what's open:
//!   ← {"op":"members","list":[{"id":1,"name":"Kit","host":true,"path":"host"},…]}
//!   ← {"op":"book","book":[{"name":"Kit","code":"kitty:hub:…","at":<secs>}],"gone":["kitty:hub:…"],"goneAt":[{"code":…,"at":…}]}
//!   ← {"op":"offers","list":[{"from":1,"name":"Kit","oid":"voip","kind":"voip","code":"kitty:voip:…","title":"Hangout"}]}
//!   → {"op":"path","path":"relay","rtt":80}
//!   → {"op":"invite","to":[3,4],"code":"kitty:voip:…","title":"Kit's room"}   ([] for everyone)
//!   ← {"op":"invite","inv":7,"from":1,"name":"Kit","kind":"voip","code":"kitty:voip:…","title":"Kit's room"}
//!   ← {"op":"invited","n":2}
//!   → {"op":"address","code":"kitty:hub:…"}       (saved hubs: our own code for it)
//!   → {"op":"merge","book":[…],"gone":[…],"goneAt":[…]}   (saved hubs: our copy of the book;
//!                                                    newest wins, code by code)
//!   → {"op":"forget","code":"kitty:hub:…"}        (we deleted our copy)
//!   → {"op":"offer","oid":"voip","code":"kitty:voip:…","title":"Hangout"}   ("Open to hub")
//!   → {"op":"withdraw","oid":"voip"}
//!   → {"op":"delete"}  ← {"op":"deleted","by":"Sam"}   (delete it for everyone)
//!   ← {"op":"moving","code":"kitty:hub:…","name":"Sam"}  (a saved hub's host closed it:
//!                                    Sam's copy takes over, and everyone goes there)
//!   → {"op":"kick","id":4}                                   (host only)
//!   ← {"op":"kicked"}  (then the hub closes the connection)
//!   → {"op":"ping"}  ← {"op":"pong"}                          (every 15 s; timed for the ping shown)
//!
//! The game API (0.16.0 on; the welcome says "games":1), for games on
//! members' PCs, through [`crate::api`]:
//!
//!   → {"op":"session","sid":"a1b2","app":"my-game","title":"Kit's lobby","port":7777,"addr":"tc…","info":{…}}
//!   → {"op":"unsession","sid":"a1b2"}
//!   ← {"op":"sessions","list":[{"from":1,"name":"Kit","sid":…,"app":…,"title":…,"port":…,"addr":…,"info":…}]}
//!     (the session's own tunnel, from the VOIP helper: UDP and TCP)
//!   → {"op":"app","app":"my-game","to":[3],"data":{…}}   ([] for everyone)
//!   ← {"op":"app","from":1,"name":"Kit","app":"my-game","data":{…}}
//!   → {"op":"activity","app":"My Game","text":"In a lobby"}   (no app: cleared;
//!                                    shown as "activity" in the members list)
//!   → {"op":"bye"}
//!
//! Scan sends one line instead of a hello, and gets one back:
//!
//!   → {"op":"peek","v":2}  ← {"op":"peeked","v":2,"id":…,"hub":…,"host":"Kit","members":3,"expiresIn":…,
//!                             "book":[…],"gone":[…],"goneAt":[…]}   (a saved hub's book, so a scan catches up)

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::code::{self, Kind};
use crate::hubstore::{self, Entry, Gone, Saved, Store};
use crate::tailcat::{self, Link, Proc, Role, TcEvent};
use crate::{Log, UserEvent};

mod app;
mod client;
mod events;
mod scan;
mod server;
#[cfg(test)]
mod tests;

pub use app::*;
pub use client::*;
pub use scan::*;
pub use server::*;

pub const PROTOCOL: u64 = 2;
/// The most people in one hub.
pub const MAX_MEMBERS: usize = 32;
/// The longest line either side takes (an address book fits).
const MAX_LINE: u64 = 64 * 1024;
/// Things one person can have open in the hub at once ("Open to hub").
const MAX_OFFERS_EACH: usize = 16;
const MAX_TITLE: usize = 60;
/// Game sessions one person's games can host at once (the game API).
const MAX_SESSIONS_EACH: usize = 8;
/// Game messages one person may send a second, and how big each may be.
const APP_PER_SECOND: usize = 60;
const MAX_APP_DATA: usize = 16 * 1024;
/// What the welcome says this hub does beyond PROTOCOL: 1 = game sessions,
/// game messages and activities (0.16.0). Older hubs leave it out, and
/// ignore those messages.
const GAMES: u64 = 1;
/// Invites one person may send a minute; more are refused.
const INVITES_PER_MINUTE: usize = 12;
/// Each side pings every this often, and gives up after a few missed.
const PING_EVERY: Duration = Duration::from_secs(5);
/// Silence for this long (5 missed pings) means the other end is gone: a
/// PC that restarted or lost its connection says nothing. 0.12.2 and older
/// ping every 15 s, so they still fit.
const SILENT_FOR: Duration = Duration::from_secs(25);
/// A friend's hello can take a while: the tunnel comes up first.
const HELLO_WAIT: Duration = Duration::from_secs(25);

/// Printable, trimmed, length-capped (names, hub names, invite titles).
pub fn clean(s: &str, max: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    s.trim().chars().take(max).collect::<String>().trim().to_string()
}

/// What an invite is to, from its code, and the code written out clean
/// (nothing but the code travels): only codes for things a hub can invite
/// to. Port codes need their port numbers in them; a bare address can't be
/// joined without being told them.
pub fn invitation(code_text: &str) -> Option<(&'static str, String)> {
    let c = code::parse(code_text).ok()?;
    let kind = match c.kind {
        Kind::Voip => "voip",
        Kind::File => "file",
        Kind::Game => "game",
        Kind::Mount => "mount",
        Kind::Port => "port",
        Kind::Bare | Kind::Hub => return None,
    };
    Some((kind, code::canonical(&c)?))
}

#[cfg(test)]
fn invite_kind(code_text: &str) -> Option<&'static str> {
    invitation(code_text).map(|(k, _)| k)
}

/// Two secrets compared without stopping at the first difference.
pub fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Writes one line; false if the connection is gone.
fn send_line(stream: &mut TcpStream, v: &Value) -> bool {
    let mut line = v.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).and_then(|_| stream.flush()).is_ok()
}

/// Reads one line of at most MAX_LINE bytes. None at the end, on an error,
/// on a timeout, or if the line is too long.
fn read_line(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut buf = Vec::new();
    let n = reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf).ok()?;
    if n == 0 || buf.last() != Some(&b'\n') {
        return None;
    }
    serde_json::from_slice(&buf).ok().or(Some(Value::Null))
}
