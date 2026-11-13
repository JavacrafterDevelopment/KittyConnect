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

use std::path::PathBuf;

/// Every program of ours Windows might have a rule for.
pub fn programs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::current_exe().into_iter().collect();
    if let Ok(entries) = std::fs::read_dir(crate::portable::data_dir().join("bin")) {
        let mut exes: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
            .collect();
        exes.sort();
        out.extend(exes);
    }
    out
}

/// One `netsh` per program, joined for a single elevated `cmd /c`.
pub fn command(programs: &[PathBuf]) -> String {
    programs
        .iter()
        .map(|p| format!("netsh advfirewall firewall delete rule name=all program=\"{}\"", p.display()))
        .collect::<Vec<_>>()
        .join(" & ")
}

/// Runs the removal as administrator and waits for it. Blocks: call it off the
/// UI thread. Err if the UAC prompt was declined or it couldn't run.
#[cfg(windows)]
pub fn reset() -> Result<usize, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let programs = programs();
    // /s: cmd strips only the outer quotes and keeps the quoted paths inside.
    let params = format!("/s /c \"{}\"", command(&programs));
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas"), wide("cmd.exe"), wide(&params));
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
                "Cancelled. Windows needs an administrator's OK to change firewall rules.".into()
            } else {
                format!("Couldn't run it: {}", std::io::Error::last_os_error())
            });
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, INFINITE);
            CloseHandle(info.hProcess);
        }
    }
    Ok(programs.len())
}

#[cfg(not(windows))]
pub fn reset() -> Result<usize, String> {
    Err("Only on Windows.".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_each_path() {
        let cmd = super::command(&[r"C:\Users\A B\KittyConnect.exe".into(), r"C:\x\data\bin\tailcat-0.7.0.exe".into()]);
        assert_eq!(
            cmd,
            r#"netsh advfirewall firewall delete rule name=all program="C:\Users\A B\KittyConnect.exe" & netsh advfirewall firewall delete rule name=all program="C:\x\data\bin\tailcat-0.7.0.exe""#
        );
    }
}
