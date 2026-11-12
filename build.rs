// TailCat is embedded into KittyConnect.exe, so the build needs the binary in
// vendor\. build.ps1 downloads it from the official release and checks its hash.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/tailcat.exe");
    if !std::path::Path::new("vendor/tailcat.exe").exists() {
        panic!("vendor\\tailcat.exe is missing. Run build.ps1, which downloads and verifies it.");
    }
}
