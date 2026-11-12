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

/// Where received files land unless the user picks another folder.
pub fn default_inbox() -> PathBuf {
    let beside = app_dir().join("Received");
    if std::fs::create_dir_all(&beside).is_ok() && writable(&beside) {
        return beside;
    }
    data_dir().join("Received")
}

fn writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".kc-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
