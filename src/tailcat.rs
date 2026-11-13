//! Runs TailCat.
//!
//! Two TailCat programs are embedded in KittyConnect and unpacked into
//! `data/bin/` on first use:
//!
//! - TailCat itself, for Port Forward and File Share, so those get TailCat's
//!   security updates by bumping its version. On Windows it's the official
//!   `tailcat.exe` (hash-pinned); TailCat publishes no macOS build, so on a Mac
//!   it's built from TailCat's source at the same version with its official
//!   build tags (`tools/build-tailcat-macos.sh`), as Homebrew does;
//! - `kc-voip`, built from `voip-helper/` on the TailCat library (the same
//!   TailCat version), for VOIP rooms only. It takes the same commands but
//!   carries UDP as well as TCP, which voice and screen video need. When the
//!   official tailcat can serve UDP itself, rooms can go back to it.
//!
//! Each gets its own home folder (`data/tailcat-home/`, `data/voip-home/`), so
//! neither reads or writes keys in the user's profile or in the other's. Every
//! child dies with us: on Windows through a job object, on macOS through a
//! small watchdog (see `reaper`), even if KittyConnect crashes.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use tao::event_loop::EventLoopProxy;

use crate::portable::data_dir;
use crate::UserEvent;

/// The TailCat version of both programs. The VOIP helper's `go.mod` must name
/// the same one (the build checks).
pub const VERSION: &str = "0.7.0";
#[cfg(windows)]
static OFFICIAL: &[u8] = include_bytes!("../vendor/tailcat.exe");
#[cfg(windows)]
static VOIP: &[u8] = include_bytes!("../vendor/kc-voip.exe");
#[cfg(target_os = "macos")]
static OFFICIAL: &[u8] = include_bytes!("../vendor/macos/tailcat");
#[cfg(target_os = "macos")]
static VOIP: &[u8] = include_bytes!("../vendor/macos/kc-voip");
// Other systems aren't shipped; a build there (for the tests) embeds nothing.
#[cfg(not(any(windows, target_os = "macos")))]
static OFFICIAL: &[u8] = &[];
#[cfg(not(any(windows, target_os = "macos")))]
static VOIP: &[u8] = &[];

/// Program files end in `.exe` on Windows and have no extension elsewhere.
const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// Which of the two embedded programs to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binary {
    /// The official tailcat.exe: TCP only.
    Official,
    /// KittyConnect's helper on the TailCat library: TCP and UDP.
    Voip,
}

impl Binary {
    fn bytes(self) -> &'static [u8] {
        match self {
            Binary::Official => OFFICIAL,
            Binary::Voip => VOIP,
        }
    }

    fn file_name(self) -> String {
        match self {
            Binary::Official => format!("tailcat-{VERSION}{EXE}"),
            // Rebuilt with each KittyConnect version, so named by both.
            Binary::Voip => format!("kc-voip-{}-tc{VERSION}{EXE}", env!("CARGO_PKG_VERSION")),
        }
    }

    fn home_name(self) -> &'static str {
        match self {
            Binary::Official => "tailcat-home",
            Binary::Voip => "voip-home",
        }
    }
}

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

impl Role {
    /// Rooms run the helper (they need UDP); everything else runs the
    /// official tailcat.exe.
    pub fn binary(self) -> Binary {
        match self {
            Role::VoipHost | Role::VoipJoin => Binary::Voip,
            Role::ShareHost | Role::RecvForward | Role::PortHost | Role::PortJoin => Binary::Official,
        }
    }
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
    /// A harmless stand-in process, for tests of what runs TailCat.
    #[cfg(test)]
    pub fn stand_in() -> Proc {
        let child = if cfg!(windows) {
            Command::new("cmd").args(["/c", "ping -n 60 127.0.0.1 >nul"]).spawn()
        } else {
            Command::new("sleep").arg("60").spawn()
        };
        Proc { id: 0, child: child.expect("a stand-in process") }
    }

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

/// Unpacks an embedded program if this version isn't there yet.
fn exe_path(bin: Binary) -> Result<PathBuf, String> {
    let bytes = bin.bytes();
    let dir = data_dir().join("bin");
    let exe = dir.join(bin.file_name());
    if std::fs::metadata(&exe).map(|m| m.len() == bytes.len() as u64).unwrap_or(false) {
        return Ok(exe);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{}.part", bin.file_name()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("Couldn't unpack TailCat: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Couldn't unpack TailCat: {e}"))?;
    }
    let _ = std::fs::remove_file(&exe);
    std::fs::rename(&tmp, &exe).map_err(|e| format!("Couldn't unpack TailCat: {e}"))?;
    Ok(exe)
}

/// Unpacks both programs ahead of time so the first connection isn't slowed by it.
pub fn prepare() -> Result<(), String> {
    exe_path(Binary::Official)?;
    exe_path(Binary::Voip).map(|_| ())
}

fn home(bin: Binary) -> PathBuf {
    let h = data_dir().join(bin.home_name());
    let _ = std::fs::create_dir_all(h.join(".config"));
    h
}

pub fn spawn(role: Role, args: &[String], proxy: &EventLoopProxy<UserEvent>) -> Result<Proc, String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let bin = role.binary();
    let exe = exe_path(bin)?;
    let home = home(bin);

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
    #[cfg(unix)]
    reaper::adopt(child.id());

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
    /// The path we last told the far end about (see [`crate::peers`]); when
    /// it changes, the next check goes at once.
    pub reported: &'static str,
    /// Just reconnected to try for a direct path: checks that fail before this
    /// are the tunnel coming back up, not the far end leaving.
    pub grace_until: Option<std::time::Instant>,
}

pub const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
/// More often while on a relay, so a host's "Retry direct" reaches us quickly.
pub const PING_RELAY: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a reconnect gets to come back before failed checks count.
pub const RECONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(45);
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

    /// The tunnel was just restarted (to try for a direct path again): the
    /// path is unknown until TailCat says, and the far end gets a moment to answer.
    pub fn restarted(&mut self) {
        let now = std::time::Instant::now();
        self.direct = None;
        self.peer_seen = false;
        self.rtt_ms = None;
        self.fails = 0;
        self.pinging = false;
        self.next_ping = Some(now + std::time::Duration::from_secs(3));
        self.grace_until = Some(now + RECONNECT_GRACE);
    }

    pub fn ping_due(&self, now: std::time::Instant) -> bool {
        !self.pinging && self.next_ping.is_none_or(|t| now >= t)
    }

    pub fn ping_started(&mut self) {
        self.pinging = true;
        self.reported = self.path();
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
                self.grace_until = None;
                self.next_ping = Some(now + if self.direct.is_some() { PING_EVERY } else { PING_RELAY });
                false
            }
            Err(_) if self.grace_until.is_some_and(|t| now < t) => {
                self.next_ping = Some(now + std::time::Duration::from_secs(3));
                false
            }
            Err(_) => {
                self.fails = self.fails.saturating_add(1);
                // Until the far end has answered once, keep asking every few
                // seconds: a friend who comes online shows up quickly.
                self.next_ping = Some(now + if self.fails == 1 || !self.ok_once { PING_RETRY } else { PING_EVERY });
                self.ok_once && self.fails >= 2
            }
        }
    }

    /// "direct", "relay", or "unknown" until the far end has been seen.
    pub fn path(&self) -> &'static str {
        match (&self.direct, self.peer_seen) {
            (Some(_), _) => "direct",
            (None, true) => "relay",
            (None, false) => "unknown",
        }
    }

    /// The far end should hear about a change of path now, not in 30 seconds.
    pub fn report_due(&self) -> bool {
        self.ok_once && self.path() != self.reported
    }

    pub fn json(&self) -> serde_json::Value {
        let kind = self.path();
        serde_json::json!({
            "path": kind,
            "endpoint": self.direct,
            "rtt": self.rtt_ms,
            "checkedAgo": self.checked.map(|t| t.elapsed().as_secs()),
        })
    }
}

/// What came back from a check.
pub struct Pong {
    pub rtt: std::time::Duration,
    /// The host's name; older versions don't send one.
    pub name: Option<String>,
    /// The host pressed "Retry direct" for us.
    pub retry: bool,
}

/// One request/reply check through a forwarded port: `request` (a
/// [`crate::peers::Report`] line) → a line containing `"pong"`. Answered by
/// the file server and the port beacon on the far side, so a reply means the
/// whole tunnel is up.
pub fn ping_port(port: u16, request: &str, timeout: std::time::Duration) -> Result<Pong, String> {
    use std::io::Write;
    let t0 = std::time::Instant::now();
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(timeout)).ok();
    s.write_all(format!("{}\n", request.trim_end()).as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s.take(4096)).read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.contains("pong") {
        return Err("no answer".into());
    }
    let v = serde_json::from_str::<serde_json::Value>(line.trim()).unwrap_or_default();
    let name = v["name"].as_str().map(crate::peers::clean_name).filter(|n| !n.is_empty());
    Ok(Pong { rtt: t0.elapsed(), name, retry: v["retry"] == true })
}

/// Tells the far end we're leaving, so we drop off its list at once. Best
/// effort: a short timeout, and no answer is expected.
pub fn say_bye(port: u16, request: &str) {
    use std::io::Write;
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    if let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)) {
        let _ = s.write_all(format!("{}\n", request.trim_end()).as_bytes());
        let _ = s.flush();
        s.set_read_timeout(Some(std::time::Duration::from_millis(800))).ok();
        let _ = s.read(&mut [0u8; 64]);
    }
}

/// Ends TailCat children left behind by an earlier run that crashed (and whose
/// watchdog didn't get to them). Nothing to do on Windows, where the job
/// object ends them with us.
pub fn sweep_leftovers() {
    #[cfg(unix)]
    reaper::sweep();
}

/// macOS has no job objects, so a watchdog stands in: a tiny `sh` in its own
/// process group that waits for KittyConnect to exit, however it exits, then
/// ends every TailCat child it started (listed in `data/run/<pid>.pids`).
/// Only processes whose program is in `data/bin/` are ever ended, so a reused
/// process id can't take anything else down. A crash would otherwise leave an
/// opened port reachable through its code.
#[cfg(unix)]
mod reaper {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    pub(super) const SCRIPT: &str = r#"while kill -0 "$1" 2>/dev/null; do sleep 1; done
[ -f "$2" ] || exit 0
while read -r p; do
  case "$(ps -p "$p" -o command= 2>/dev/null)" in
    "$3"/*) kill -9 "$p" 2>/dev/null ;;
  esac
done < "$2"
rm -f "$2""#;

    fn run_dir() -> PathBuf {
        crate::portable::data_dir().join("run")
    }

    fn bin_dir() -> PathBuf {
        crate::portable::data_dir().join("bin")
    }

    fn list(pid: u32) -> PathBuf {
        run_dir().join(format!("{pid}.pids"))
    }

    pub(super) fn watch(parent: u32, list: &Path, bin: &Path) -> std::io::Result<std::process::Child> {
        use std::os::unix::process::CommandExt;
        Command::new("/bin/sh")
            .arg("-c")
            .arg(SCRIPT)
            .arg("kittyconnect-watchdog")
            .arg(parent.to_string())
            .arg(list)
            .arg(bin)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own group, so a Ctrl+C meant for KittyConnect doesn't end it too.
            .process_group(0)
            .spawn()
    }

    /// Notes a TailCat child, starting the watchdog with the first one.
    pub fn adopt(pid: u32) {
        static WATCHDOG: OnceLock<()> = OnceLock::new();
        let me = std::process::id();
        let _ = std::fs::create_dir_all(run_dir());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(list(me)) {
            let _ = writeln!(f, "{pid}");
        }
        WATCHDOG.get_or_init(|| {
            let _ = watch(me, &list(me), &bin_dir());
        });
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Ends the TailCat children of earlier runs that are gone.
    pub fn sweep() {
        let Ok(entries) = std::fs::read_dir(run_dir()) else { return };
        let me = std::process::id().to_string();
        let bin = format!("{}/", bin_dir().display());
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(owner) = name.strip_suffix(".pids") else { continue };
            if owner == me || !owner.chars().all(|c| c.is_ascii_digit()) || alive(owner) {
                continue;
            }
            for pid in std::fs::read_to_string(e.path()).unwrap_or_default().lines().map(str::trim) {
                if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let command = Command::new("ps")
                    .args(["-p", pid, "-o", "command="])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                if command.starts_with(&bin) {
                    let _ = Command::new("kill").args(["-9", pid]).status();
                }
            }
            let _ = std::fs::remove_file(e.path());
        }
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

    /// Rooms run the UDP-capable helper; Ports and File Share stay on the
    /// official tailcat.exe, each with its own home folder.
    #[test]
    fn rooms_use_the_helper_and_the_rest_the_official_build() {
        for r in [Role::VoipHost, Role::VoipJoin] {
            assert_eq!(r.binary(), Binary::Voip);
        }
        for r in [Role::ShareHost, Role::RecvForward, Role::PortHost, Role::PortJoin] {
            assert_eq!(r.binary(), Binary::Official);
        }
        assert_ne!(Binary::Voip.file_name(), Binary::Official.file_name());
        assert_ne!(Binary::Voip.home_name(), Binary::Official.home_name());
    }

    /// The watchdog ends a child in `bin/` once its parent is gone, and
    /// leaves anything else alone.
    #[cfg(unix)]
    #[test]
    fn watchdog_ends_children_when_the_app_dies() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("kc-reaper-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sleeper = bin.join("tailcat-test");
        // A link, not a copy: a copy still open for writing while another test
        // starts a process can't be run ("text file busy").
        let _ = std::fs::remove_file(&sleeper);
        std::os::unix::fs::symlink("/bin/sleep", &sleeper).unwrap();

        let mut ours = Command::new(&sleeper).arg("60").spawn().unwrap();
        let mut other = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        // Stands in for KittyConnect: exits after a moment.
        let mut app = Command::new("/bin/sleep").arg("1").spawn().unwrap();
        let list = dir.join("app.pids");
        std::fs::write(&list, format!("{}\n{}\n", ours.id(), other.id())).unwrap();
        let mut dog = reaper::watch(app.id(), &list, &bin).unwrap();

        app.wait().unwrap();
        // The watchdog notices within a second and ends only the program in bin/.
        let _ = dog.wait();
        let gone = ours.wait().unwrap();
        assert!(!gone.success(), "the child in bin/ was ended");
        assert!(other.try_wait().unwrap().is_none(), "anything else is left running");
        assert!(!list.exists());
        let _ = other.kill();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_names_fit_the_system() {
        let name = Binary::Official.file_name();
        assert_eq!(name.ends_with(".exe"), cfg!(windows), "{name}");
        assert!(name.starts_with("tailcat-") && Binary::Voip.file_name().starts_with("kc-voip-"));
    }

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
        // Told the far end "unknown" when the check went out: tell it "relay" now.
        assert!(l.report_due());
        l.ping_started();
        assert!(!l.report_due());
        assert!(!l.ping_result(Err("x".into())));
        assert!(l.ping_result(Err("x".into())));
    }

    #[test]
    fn asks_often_until_the_far_end_first_answers() {
        let mut l = Link::default();
        for _ in 0..4 {
            let t = std::time::Instant::now();
            l.ping_result(Err("no answer".into()));
            assert!(l.next_ping.unwrap() - t <= PING_RETRY + std::time::Duration::from_millis(50));
        }
        l.ping_result(Ok(std::time::Duration::from_millis(20)));
        l.ping_result(Err("x".into()));
        let t = std::time::Instant::now();
        l.ping_result(Err("x".into()));
        assert!(l.next_ping.unwrap() - t > PING_RETRY, "once it has answered, every 30 seconds");
    }

    #[test]
    fn a_reconnect_is_not_mistaken_for_the_far_end_leaving() {
        let mut l = Link::default();
        l.ping_result(Ok(std::time::Duration::from_millis(20)));
        assert!(l.observe("2026/11/12 01:02:03 magicsock: disco: node [a] d:1 now using 10.0.0.5:41641 mtu=1360"));
        l.restarted();
        assert_eq!(l.path(), "unknown");
        for _ in 0..5 {
            assert!(!l.ping_result(Err("tunnel coming back".into())));
        }
        assert!(!l.ping_result(Ok(std::time::Duration::from_millis(30))));
        assert_eq!(l.path(), "relay");
        // Back to normal: two misses now mean it's gone.
        assert!(!l.ping_result(Err("x".into())));
        assert!(l.ping_result(Err("x".into())));
    }
}
