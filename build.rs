// Both TailCat programs are embedded into KittyConnect, so the build needs
// them in vendor/:
//
// - Windows: the official tailcat.exe (build.ps1 downloads it and checks its
//   hash) and kc-voip.exe (build.ps1 builds it from voip-helper/ with Go).
// - macOS: tailcat and kc-voip, both built from source by
//   tools/build-tailcat-macos.sh (build-macos.sh runs it; needs Go).
//
// Other systems aren't shipped and embed nothing (the tests still run there).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let need: &[(&str, &str)] = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => &[
            ("vendor/tailcat.exe", "Run build.ps1, which downloads and verifies it."),
            ("vendor/kc-voip.exe", "Run build.ps1, which builds it from voip-helper (needs Go)."),
        ],
        Ok("macos") => &[
            ("vendor/macos/tailcat", "Run tools/build-tailcat-macos.sh (needs Go)."),
            ("vendor/macos/kc-voip", "Run tools/build-tailcat-macos.sh (needs Go)."),
        ],
        _ => &[],
    };
    for (path, fix) in need {
        println!("cargo:rerun-if-changed={path}");
        if !std::path::Path::new(path).exists() {
            panic!("{path} is missing. {fix}");
        }
    }
}
