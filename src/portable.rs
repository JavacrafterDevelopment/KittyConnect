//! Everything KittyConnect writes lives beside the program, so the folder can
//! be carried on a stick or zipped to a friend. Nothing goes in the user
//! profile, except on a Mac when there's nowhere writable beside the app.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The folder holding KittyConnect.exe.
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where KittyConnect keeps its data, decided once per run: the first of
/// [`data_candidates`] that can be written to, or a temp folder if none can
/// (a read-only share, a CD).
pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        for dir in data_candidates() {
            if std::fs::create_dir_all(&dir).is_ok() && writable(&dir) {
                return dir;
            }
        }
        let tmp = std::env::temp_dir().join("KittyConnect");
        let _ = std::fs::create_dir_all(&tmp);
        tmp
    })
    .clone()
}

/// Windows: `data\` beside KittyConnect.exe.
#[cfg(not(target_os = "macos"))]
fn data_candidates() -> Vec<PathBuf> {
    vec![app_dir().join("data")]
}

#[cfg(target_os = "macos")]
fn data_candidates() -> Vec<PathBuf> {
    mac_data_candidates(
        &std::env::current_exe().unwrap_or_default(),
        std::env::var_os("KITTYCONNECT_DATA_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// macOS, in order:
/// 1. `KITTYCONNECT_DATA_DIR`, which the Javacrafter Launcher sets to
///    `KittyConnectData` beside the app it installed;
/// 2. `KittyConnectData` beside `KittyConnect.app`, so the app and its data
///    move together like the Windows folder. Writing inside the app itself
///    would break its signature. Skipped when macOS runs the app from a
///    read-only copy (App Translocation, for an app opened straight from
///    Downloads);
/// 3. `data` beside the program when it isn't in an app bundle (a dev build);
/// 4. `~/Library/Application Support/KittyConnect`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_data_candidates(exe: &Path, env_dir: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = env_dir.filter(|d| d.is_absolute()).into_iter().collect();
    let dir = exe.parent().unwrap_or(Path::new("/"));
    // …/KittyConnect.app/Contents/MacOS/KittyConnect
    let bundle = dir
        .parent()
        .and_then(Path::parent)
        .filter(|b| dir.ends_with("Contents/MacOS") && b.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")));
    match bundle {
        Some(b) if !b.to_string_lossy().contains("/AppTranslocation/") => {
            if let Some(beside) = b.parent() {
                out.push(beside.join("KittyConnectData"));
            }
        }
        Some(_) => {}
        None => out.push(dir.join("data")),
    }
    if let Some(home) = home {
        out.push(home.join("Library/Application Support/KittyConnect"));
    }
    out
}

/// Where received files land unless the user picks another folder: the user's
/// Downloads folder, wherever Windows has it (it can be moved to another
/// drive). Falls back to `Received\` beside the exe if there's no Downloads.
pub fn default_inbox() -> PathBuf {
    if let Some(dir) = downloads_dir().filter(|d| d.is_dir()) {
        return dir;
    }
    // Not inside a Mac app bundle, which would break its signature.
    if !cfg!(target_os = "macos") {
        let beside = app_dir().join("Received");
        if std::fs::create_dir_all(&beside).is_ok() && writable(&beside) {
            return beside;
        }
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

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".kc-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::mac_data_candidates;
    use std::path::{Path, PathBuf};

    #[test]
    fn mac_data_goes_beside_the_app() {
        let home = Some(PathBuf::from("/Users/sam"));
        let support = PathBuf::from("/Users/sam/Library/Application Support/KittyConnect");
        let exe = Path::new("/Users/sam/Apps/KittyConnect.app/Contents/MacOS/KittyConnect");
        assert_eq!(mac_data_candidates(exe, None, home.clone()), vec![PathBuf::from("/Users/sam/Apps/KittyConnectData"), support.clone()]);

        // The Launcher says where.
        let launcher = PathBuf::from("/Users/sam/Launcher/portableApplications/KittyConnect/KittyConnectData");
        assert_eq!(mac_data_candidates(exe, Some(launcher.clone()), home.clone())[0], launcher);

        // Opened straight from Downloads: macOS runs a read-only copy.
        let moved = Path::new("/private/var/folders/x/AppTranslocation/ABC/d/KittyConnect.app/Contents/MacOS/KittyConnect");
        assert_eq!(mac_data_candidates(moved, None, home.clone()), vec![support]);

        // A dev build, not in a bundle.
        let dev = Path::new("/Users/sam/src/KittyConnect/target/release/KittyConnect");
        assert_eq!(mac_data_candidates(dev, None, None), vec![PathBuf::from("/Users/sam/src/KittyConnect/target/release/data")]);
    }

    #[test]
    fn finds_downloads() {
        let d = super::downloads_dir().expect("a Downloads folder");
        assert!(d.is_absolute(), "{}", d.display());
        assert!(super::default_inbox().is_dir());
    }
}
