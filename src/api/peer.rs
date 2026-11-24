//! Which program is on the other end of a connection to the game API, so
//! the "Allow?" prompt can show its file, and a key only works for the
//! program it was given to.
//!
//! Windows: the TCP table (GetExtendedTcpTable, iphlpapi) names the process
//! that owns the connection's port, and QueryFullProcessImageNameW its file.
//! macOS: `lsof` (part of macOS) names the process, and proc_pidpath its
//! file. Elsewhere (the tests' Linux), nobody is named.

/// The full path of the program connected to our port `ours` from
/// 127.0.0.1:`theirs`, if the OS says.
pub fn program(theirs: u16, ours: u16) -> Option<String> {
    imp::program(theirs, ours)
}

/// Just the file name of a program's path, for the prompt.
pub fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    const AF_INET: u32 = 2;
    const TCP_TABLE_OWNER_PID_ALL: i32 = 5;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetExtendedTcpTable(table: *mut c_void, size: *mut u32, order: i32, af: u32, class: i32, reserved: u32) -> u32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn QueryFullProcessImageNameW(process: *mut c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// A port as the TCP table holds it: network order, in the low 16 bits.
    fn port(v: u32) -> u16 {
        u16::from_be(v as u16)
    }

    fn pid(theirs: u16, ours: u16) -> Option<u32> {
        let mut size = 0u32;
        // SAFETY: a null table asks for the size needed.
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0) };
        for _ in 0..4 {
            // MIB_TCPTABLE_OWNER_PID: a count, then rows of six u32s
            // (state, local address, local port, remote address, remote port, pid).
            let mut buf = vec![0u32; size as usize / 4 + 64];
            let mut len = (buf.len() * 4) as u32;
            // SAFETY: `buf` is `len` bytes, 4-byte aligned.
            let r = unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut len, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0) };
            if r == ERROR_INSUFFICIENT_BUFFER {
                size = len;
                continue;
            }
            if r != 0 {
                return None;
            }
            let n = (buf[0] as usize).min((buf.len() - 1) / 6);
            return buf[1..1 + n * 6].chunks_exact(6).find(|row| port(row[2]) == theirs && port(row[4]) == ours).map(|row| row[5]);
        }
        None
    }

    pub fn program(theirs: u16, ours: u16) -> Option<String> {
        let pid = pid(theirs, ours)?;
        // SAFETY: plain calls; the handle is closed below, the buffer is `len` u16s.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(h);
            ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::process::{Command, Stdio};

    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }

    pub fn program(theirs: u16, _ours: u16) -> Option<String> {
        // Both ends of the connection are on this PC: ours, and theirs.
        let out = Command::new("/usr/sbin/lsof")
            .args(["-nP", &format!("-iTCP@127.0.0.1:{theirs}"), "-sTCP:ESTABLISHED", "-Fp"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let me = std::process::id() as i32;
        let pid = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_prefix('p')?.parse::<i32>().ok()).find(|p| *p != me)?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `buf` is that many bytes.
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).to_string())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn program(_theirs: u16, _ours: u16) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names() {
        assert_eq!(super::file_name(r"C:\Games\My Game\game.exe"), "game.exe");
        assert_eq!(super::file_name("/Applications/My Game.app/Contents/MacOS/My Game"), "My Game");
        assert_eq!(super::file_name("game"), "game");
    }
}
