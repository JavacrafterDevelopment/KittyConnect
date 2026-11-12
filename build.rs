// Both TailCat programs are embedded into KittyConnect.exe, so the build needs
// them in vendor\: the official tailcat.exe (build.ps1 downloads it and checks
// its hash) and kc-voip.exe, the VOIP rooms' helper (build.ps1 builds it from
// voip-helper\ with Go).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/tailcat.exe");
    println!("cargo:rerun-if-changed=vendor/kc-voip.exe");
    if !std::path::Path::new("vendor/tailcat.exe").exists() {
        panic!("vendor\\tailcat.exe is missing. Run build.ps1, which downloads and verifies it.");
    }
    if !std::path::Path::new("vendor/kc-voip.exe").exists() {
        panic!("vendor\\kc-voip.exe is missing. Run build.ps1, which builds it from voip-helper\\ (needs Go).");
    }
}
