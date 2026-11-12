//! Runs the bundled TailCat binary.
//!
//! The official Windows build is embedded in KittyConnect.exe and unpacked into
//! `data\bin\` on first use. Each run gets its config/home pointed at
//! `data\tailcat-home\`, so TailCat never reads or writes keys in the user's
//! profile, and every child is tied to a job object so it dies with us.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use tao::event_loop::EventLoopProxy;

use crate::portable::data_dir;
use crate::UserEvent;

pub const VERSION: &str = "0.7.0";
static BINARY: &[u8] = include_bytes!("../vendor/tailcat.exe");

/// Which part of the app a TailCat process belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    ShareHost,
    RecvForward,
    PortHost,
    PortJoin,
    VoipHost,
    VoipJoin,
}

pub enum TcEvent {
    Line(String),
    Exited,
}

pub struct Proc {
    pub id: u64,
    child: Child,
}

impl Proc {
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|s| s.code())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Unpacks the embedded binary if this version isn't there yet.
fn exe_path() -> Result<PathBuf, String> {
    let dir = data_dir().join("bin");
    let exe = dir.join(format!("tailcat-{VERSION}.exe"));
    if std::fs::metadata(&exe).map(|m| m.len() == BINARY.len() as u64).unwrap_or(false) {
        return Ok(exe);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("tailcat-{VERSION}.exe.part"));
    std::fs::write(&tmp, BINARY).map_err(|e| format!("Couldn't unpack TailCat: {e}"))?;
    let _ = std::fs::remove_file(&exe);
    std::fs::rename(&tmp, &exe).map_err(|e| format!("Couldn't unpack TailCat: {e}"))?;
    Ok(exe)
}

/// Unpacks TailCat ahead of time so the first connection isn't slowed by it.
pub fn prepare() -> Result<(), String> {
    exe_path().map(|_| ())
}

fn home() -> PathBuf {
    let h = data_dir().join("tailcat-home");
    let _ = std::fs::create_dir_all(h.join(".config"));
    h
}

pub fn spawn(role: Role, args: &[String], proxy: &EventLoopProxy<UserEvent>) -> Result<Proc, String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let exe = exe_path()?;
    let home = home();

    let mut cmd = Command::new(&exe);
    // --verbose (a root flag, so it goes first) is what makes TailCat log
    // "now using <ip:port>" when the tunnel goes direct; see [`Link`].
    cmd.arg("--verbose")
        .args(args)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Keep TailCat's keys and config inside the portable folder.
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", home.join(".config"))
        .env("LOCALAPPDATA", home.join(".config"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start TailCat: {e}"))?;
    #[cfg(windows)]
    job::adopt(&child);

    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Some(out) = child.stdout.take() {
        pump(out, role, id, proxy.clone(), false);
    }
    if let Some(err) = child.stderr.take() {
        pump(err, role, id, proxy.clone(), true);
    }
    Ok(Proc { id, child })
}

/// Forwards each output line to the event loop. stderr closing means the
/// process has gone, which is reported as `Exited`.
fn pump(stream: impl Read + Send + 'static, role: Role, id: u64, proxy: EventLoopProxy<UserEvent>, last: bool) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                    if !line.is_empty() {
                        let _ = proxy.send_event(UserEvent::Tc(role, id, TcEvent::Line(line)));
                    }
                }
            }
        }
        if last {
            let _ = proxy.send_event(UserEvent::Tc(role, id, TcEvent::Exited));
        }
    });
}

/// Pulls a tailcat address (`tc` + base64url) out of a line of TailCat output.
pub fn find_addr(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`')
        .map(|t| t.trim_end_matches(|c: char| c == '.' || c == ','))
        .find(|t| is_addr(t))
        .map(str::to_string)
}

pub fn is_addr(t: &str) -> bool {
    t.len() >= 40 && t.starts_with("tc") && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A Go `log.Printf` line ("2026/11/12 01:02:03 ..."), which TailCat only
/// writes with --verbose. Most are networking chatter and stay out of the
/// visible log; [`keep_verbose`] picks the ones worth showing.
pub fn is_verbose(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() > 20 && b[4] == b'/' && b[7] == b'/' && b[10] == b' ' && b[13] == b':' && b[..4].iter().all(u8::is_ascii_digit)
}

pub fn keep_verbose(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("now using") || l.contains("got meow") || l.contains("error") || l.contains("fail")
}

/// How one tunnel is carried, and whether the far end still answers.
///
/// TailCat always starts over a DERP relay and logs
/// `magicsock: disco: node … now using <ip:port>` when hole punching finds a
/// direct path. KittyConnect also pings the far end through the tunnel every
/// 30 seconds for the round-trip time and to notice when it's gone.
#[derive(Clone, Default)]
pub struct Link {
    pub direct: Option<String>,
    /// True once the far end has been seen at all (a meow, a pong, a reply).
    pub peer_seen: bool,
    pub rtt_ms: Option<u32>,
    pub checked: Option<std::time::Instant>,
    pub ok_once: bool,
    pub fails: u8,
    pub pinging: bool,
    pub next_ping: Option<std::time::Instant>,
}

pub const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
pub const PING_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

impl Link {
    /// Reads one TailCat output line. Returns true if the link changed.
    pub fn observe(&mut self, line: &str) -> bool {
        if let Some(rest) = line.split("now using ").nth(1) {
            let addr = rest.split_whitespace().next().unwrap_or("").to_string();
            // A peer-relay path (Tailscale's, not DERP) carries a VNI; it's still a relay.
            if addr.is_empty() || addr.contains("vni") {
                return false;
            }
            self.direct = Some(addr);
            self.peer_seen = true;
            return true;
        }
        if line.contains("got meow from") && !self.peer_seen {
            self.peer_seen = true;
            return true;
        }
        false
    }

    pub fn ping_due(&self, now: std::time::Instant) -> bool {
        !self.pinging && self.next_ping.is_none_or(|t| now >= t)
    }

    pub fn ping_started(&mut self) {
        self.pinging = true;
    }

    /// Records a ping result. Returns true when the far end should be treated
    /// as gone: two failures in a row after it has answered before.
    pub fn ping_result(&mut self, result: Result<std::time::Duration, String>) -> bool {
        let now = std::time::Instant::now();
        self.pinging = false;
        self.checked = Some(now);
        match result {
            Ok(rtt) => {
                self.rtt_ms = Some(rtt.as_millis().min(u32::MAX as u128) as u32);
                self.ok_once = true;
                self.peer_seen = true;
                self.fails = 0;
                self.next_ping = Some(now + PING_EVERY);
                false
            }
            Err(_) => {
                self.fails = self.fails.saturating_add(1);
                self.next_ping = Some(now + if self.fails == 1 { PING_RETRY } else { PING_EVERY });
                self.ok_once && self.fails >= 2
            }
        }
    }

    pub fn json(&self) -> serde_json::Value {
        let kind = match (&self.direct, self.peer_seen) {
            (Some(_), _) => "direct",
            (None, true) => "relay",
            (None, false) => "unknown",
        };
        serde_json::json!({
            "path": kind,
            "endpoint": self.direct,
            "rtt": self.rtt_ms,
            "checkedAgo": self.checked.map(|t| t.elapsed().as_secs()),
        })
    }
}

/// One request/reply ping through a forwarded port: `{"op":"ping"}` →
/// a line containing `"pong"`. Answered by the file server and the port
/// beacon on the far side, so a reply means the whole tunnel is up.
pub fn ping_port(port: u16, timeout: std::time::Duration) -> Result<std::time::Duration, String> {
    use std::io::Write;
    let t0 = std::time::Instant::now();
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(timeout)).ok();
    s.write_all(b"{\"op\":\"ping\"}\n").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s.take(4096)).read_line(&mut line).map_err(|e| e.to_string())?;
    if line.contains("pong") {
        Ok(t0.elapsed())
    } else {
        Err("no answer".into())
    }
}

/// A job object with kill-on-close: when KittyConnect exits, however it exits,
/// Windows ends every TailCat it started.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::*;

    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    fn job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            Some(Job(h))
        })
        .as_ref()
    }

    pub fn adopt(child: &std::process::Child) {
        if let Some(j) = job() {
            unsafe {
                AssignProcessToJobObject(j.0, child.as_raw_handle() as HANDLE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_address_in_banner() {
        let a = "tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEu";
        assert_eq!(find_addr(&format!("# 🐈 Server listening with new address: {a}")).as_deref(), Some(a));
        assert_eq!(find_addr(&format!("# 🐈 Server listening with saved key \"default\": {a}")).as_deref(), Some(a));
        assert_eq!(find_addr("# Selected bootstrap relay region 302, San Francisco"), None);
    }

    #[test]
    fn link_sees_direct_path() {
        let mut l = Link::default();
        assert_eq!(l.json()["path"], "unknown");
        let line = "2026/11/12 01:02:03 magicsock: disco: node [abc] d:123 now using 203.0.113.7:41641 mtu=1360 tx=a1b2c3";
        assert!(is_verbose(line) && keep_verbose(line));
        assert!(l.observe(line));
        assert_eq!(l.json()["path"], "direct");
        assert_eq!(l.direct.as_deref(), Some("203.0.113.7:41641"));
        assert!(!is_verbose("# forwarding 127.0.0.1:5000 -> remote 25565"));
    }

    #[test]
    fn link_gives_up_after_two_misses() {
        let mut l = Link::default();
        // Never answered (an older host without the beacon): never "gone".
        assert!(!l.ping_result(Err("x".into())));
        assert!(!l.ping_result(Err("x".into())));
        assert!(!l.ping_result(Ok(std::time::Duration::from_millis(20))));
        assert_eq!(l.json()["path"], "relay");
        assert!(!l.ping_result(Err("x".into())));
        assert!(l.ping_result(Err("x".into())));
    }
}
