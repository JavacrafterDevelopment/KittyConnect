//! "Ask for network permission again".
//!
//! The first time a program listens on the network, Windows Defender Firewall
//! asks whether to allow it, and remembers the answer as rules for that exe.
//! If the question was cancelled or missed, the program stays blocked from
//! receiving anything directly, and TailCat can only use a relay. Windows
//! never asks again while those rules exist.
//!
//! This removes the rules Windows has for KittyConnect.exe and the TailCat
//! programs it unpacked into `data\bin` (after a UAC prompt, since firewall
//! rules need an administrator), so Windows asks again the next time each one
//! starts listening. Nothing is added: what to allow stays the user's answer.

use std::path::{Path, PathBuf};

/// The TailCat programs KittyConnect unpacks into `data\bin`: the official
/// `tailcat-<version>.exe` and the unsigned helper `kc-voip-<version>.exe`
/// (every version still there, so rules left by older ones go too). Nothing
/// else in that folder counts.
pub fn is_ours(file_name: &str) -> bool {
    let n = file_name.to_ascii_lowercase();
    n.ends_with(".exe") && (n.starts_with("tailcat-") || n.starts_with("kc-voip-"))
}

/// A path to hand to `netsh` inside `cmd /c "…"`: absolute, and without the
/// characters `cmd` would read as something else (`%`, `!`) or that would end
/// the quotes (`"`). Windows paths almost never have these; one that does is
/// left alone rather than risk a different path reaching `netsh`.
pub fn is_safe(p: &Path) -> bool {
    let s = p.to_string_lossy();
    p.is_absolute() && !s.is_empty() && !s.contains(['%', '!', '"'])
}

/// `C:\…` rather than `\\?\C:\…`.
fn plain(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

/// The path as it is, and fully resolved (long names, folder links followed),
/// since a rule Windows made can hold either. Both are the same program.
fn spellings(p: PathBuf) -> [PathBuf; 2] {
    let resolved = plain(std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone()));
    [plain(p), resolved]
}

/// KittyConnect.exe and its TailCat programs, each once.
pub fn programs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::current_exe().into_iter().collect();
    if let Ok(entries) = std::fs::read_dir(crate::portable::data_dir().join("bin")) {
        let mut exes: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| is_ours(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        exes.sort();
        out.extend(exes);
    }
    let mut seen: Vec<String> = Vec::new();
    out.into_iter()
        .flat_map(spellings)
        .filter(|p| is_safe(p))
        .filter(|p| {
            let key = p.to_string_lossy().to_lowercase();
            let new = !seen.contains(&key);
            seen.push(key);
            new
        })
        .collect()
}

/// One `netsh` per program, joined for a single elevated `cmd /c`. Each one
/// deletes only the rules whose program is exactly that path (Microsoft's own
/// example for removing an app's rules); `name=all` alone, with no program,
/// would delete every rule, so a command is never made without one.
pub fn command(programs: &[PathBuf]) -> String {
    programs
        .iter()
        .filter(|p| is_safe(p))
        .map(|p| format!("netsh advfirewall firewall delete rule name=all program=\"{}\"", p.display()))
        .collect::<Vec<_>>()
        .join(" & ")
}

/// Runs the removal as administrator and waits for it. Blocks: call it off the
/// UI thread. Err if the UAC prompt was declined or it couldn't run.
#[cfg(windows)]
pub fn reset() -> Result<usize, String> {
    // Both TailCat programs unpacked, even if this is pressed the moment the app opens.
    let _ = crate::tailcat::prepare();
    let programs = programs();
    let command = command(&programs);
    if command.is_empty() {
        return Err("Couldn't find KittyConnect's programs to reset.".into());
    }
    // /d: skip any AutoRun command; /v:off: no !var! expansion; /s: strip only
    // the outer quotes, keeping the quoted paths inside.
    run_elevated("cmd.exe", &format!("/d /v:off /s /c \"{command}\""), "change firewall rules")?;
    Ok(programs.len())
}

/// Runs `file` with `params` as administrator (the UAC prompt), hidden, and
/// waits for it to finish. Returns its exit code. Blocks: call it off the UI
/// thread. `what` finishes "Windows needs an administrator's OK to …".
#[cfg(windows)]
pub fn run_elevated(file: &str, params: &str, what: &str) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas"), wide(file), wide(params));
    let mut code = 0u32;
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            const ERROR_CANCELLED: u32 = 1223;
            return Err(if GetLastError() == ERROR_CANCELLED {
                format!("Cancelled. Windows needs an administrator's OK to {what}.")
            } else {
                format!("Couldn't run it: {}", std::io::Error::last_os_error())
            });
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
    }
    Ok(code)
}

#[cfg(not(windows))]
pub fn reset() -> Result<usize, String> {
    Err("Only on Windows.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_kittyconnects_tailcat_programs() {
        assert!(is_ours("tailcat-0.7.0.exe"));
        assert!(is_ours("kc-voip-0.7.1-tc0.7.0.exe"));
        assert!(is_ours("KC-VOIP-0.6.0-tc0.7.0.EXE"));
        assert!(!is_ours("tailcat-0.7.0.exe.part"));
        assert!(!is_ours("notepad.exe"));
        assert!(!is_ours("tailcat.dll"));
        assert!(!is_ours("other-tailcat-0.7.0.exe"));
    }

    #[test]
    fn every_command_names_one_program() {
        let progs: Vec<PathBuf> = if cfg!(windows) {
            vec![r"C:\Users\Tom & Jerry\KittyConnect\KittyConnect.exe".into(), r"C:\x\data\bin\tailcat-0.7.0.exe".into()]
        } else {
            vec!["/Users/Tom & Jerry/KittyConnect/KittyConnect.exe".into(), "/x/data/bin/tailcat-0.7.0.exe".into()]
        };
        let cmd = command(&progs);
        let parts: Vec<&str> = cmd.split(" & netsh").collect();
        assert_eq!(parts.len(), 2);
        for (part, p) in parts.iter().zip(&progs) {
            assert!(part.ends_with(&format!("delete rule name=all program=\"{}\"", p.display())), "{part}");
        }
        // Nothing to reset: no command at all, never a bare `name=all`.
        assert_eq!(command(&[]), "");
    }

    #[test]
    fn paths_cmd_would_change_are_left_alone() {
        for bad in ["/a/%OS%/KittyConnect.exe", "/a/!x!/KittyConnect.exe", "relative/KittyConnect.exe"] {
            assert!(!is_safe(Path::new(bad)), "{bad}");
            assert_eq!(command(&[PathBuf::from(bad)]), "");
        }
        let ok = if cfg!(windows) { r"C:\Program Files\KittyConnect\KittyConnect.exe" } else { "/opt/Kitty Connect/KittyConnect.exe" };
        assert!(is_safe(Path::new(ok)));
    }

    #[test]
    fn the_running_program_is_listed_once() {
        let progs = programs();
        let me = plain(std::env::current_exe().unwrap());
        assert_eq!(progs.iter().filter(|p| **p == me).count(), 1);
        // Everything listed is KittyConnect.exe itself or one of its TailCat programs.
        let exe_name = me.file_name().unwrap().to_owned();
        assert!(progs.iter().all(|p| {
            let n = p.file_name().unwrap();
            n == exe_name || is_ours(&n.to_string_lossy())
        }));
        // No path twice.
        let mut keys: Vec<String> = progs.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), progs.len());
    }
}
