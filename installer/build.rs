// Points the installer at the already-built release binaries it embeds
// (target\release by default; honours CARGO_TARGET_DIR) and fails with a
// readable message instead of a bare include_bytes! error when they're missing.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("..").join("target"));
    let bins = target.join("release");
    for exe in ["proxy-tray.exe", "singbox-watch.exe"] {
        let p = bins.join(exe);
        if !p.exists() {
            panic!(
                "{} not found — build the apps first: run build-installer.cmd \
                 (or `cargo build --release` before building the installer)",
                p.display()
            );
        }
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
    println!("cargo:rustc-env=SINGBOX_BIN_DIR={}", bins.display());
}
