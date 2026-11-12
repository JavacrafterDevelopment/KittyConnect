//! Everything KittyConnect writes lives beside the exe, so the folder can be
//! carried on a stick or zipped to a friend. Nothing goes in the user profile.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The folder holding KittyConnect.exe.
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `data\` beside the exe, or a temp folder if the exe sits somewhere read-only
/// (a locked-down share, a CD). Decided once per run.
pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let beside = app_dir().join("data");
        if std::fs::create_dir_all(&beside).is_ok() && writable(&beside) {
            return beside;
        }
        let tmp = std::env::temp_dir().join("KittyConnect");
        let _ = std::fs::create_dir_all(&tmp);
        tmp
    })
    .clone()
}

/// Where received files land unless the user picks another folder: the user's
/// Downloads folder, wherever Windows has it (it can be moved to another
/// drive). Falls back to `Received\` beside the exe if there's no Downloads.
pub fn default_inbox() -> PathBuf {
    if let Some(dir) = downloads_dir().filter(|d| d.is_dir()) {
        return dir;
    }
    let beside = app_dir().join("Received");
    if std::fs::create_dir_all(&beside).is_ok() && writable(&beside) {
        return beside;
    }
    data_dir().join("Received")
}

#[cfg(windows)]
fn downloads_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Downloads, KF_FLAG_DEFAULT};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        let wide = unsafe { std::slice::from_raw_parts(raw, len) };
        Some(PathBuf::from(std::ffi::OsString::from_wide(wide)))
    } else {
        None
    };
    // The shell allocates the string even when the call fails.
    unsafe { CoTaskMemFree(raw as *const _) };
    path.or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads")))
}

#[cfg(not(windows))]
fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads"))
}

fn writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".kc-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_downloads() {
        let d = super::downloads_dir().expect("a Downloads folder");
        assert!(d.is_absolute(), "{}", d.display());
        assert!(super::default_inbox().is_dir());
    }
}
