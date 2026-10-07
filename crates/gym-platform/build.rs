//! Builds the vendored MediaRemote adapter framework on macOS.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/mediaremote-adapter");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest_dir.join("vendor/mediaremote-adapter");
    let out = cmake::Config::new(&source)
        .build_target("MediaRemoteAdapter")
        .profile("Release")
        .build();
    let framework = out.join("build/MediaRemoteAdapter.framework");
    assert!(
        framework.exists(),
        "MediaRemoteAdapter.framework was not produced at {}",
        framework.display()
    );
    // Keep the script next to the framework in the build output. A path into the source
    // tree breaks when another checkout sharing the target directory reuses this output.
    let script = out.join("mediaremote-adapter.pl");
    std::fs::copy(source.join("bin/mediaremote-adapter.pl"), &script)
        .expect("copy mediaremote-adapter.pl");
    println!(
        "cargo:rustc-env=GYM_MEDIAREMOTE_FRAMEWORK={}",
        framework.display()
    );
    println!(
        "cargo:rustc-env=GYM_MEDIAREMOTE_SCRIPT={}",
        script.display()
    );
}
