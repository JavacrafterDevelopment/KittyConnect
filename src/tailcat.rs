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
    cmd.args(args)
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
}
