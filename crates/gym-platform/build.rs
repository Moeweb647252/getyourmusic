//! Builds the vendored MediaRemote adapter framework and its test client on macOS.

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
        .build_target("all")
        .profile("Release")
        .build();
    let framework = out.join("build/MediaRemoteAdapter.framework");
    assert!(
        framework.exists(),
        "MediaRemoteAdapter.framework was not produced at {}",
        framework.display()
    );
    // The adapter's self-test starts this client when nothing is playing, so that it has
    // something to read.
    let test_client = out.join("build/MediaRemoteAdapterTestClient");
    assert!(
        test_client.is_file(),
        "MediaRemoteAdapterTestClient was not produced at {}",
        test_client.display()
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
    println!(
        "cargo:rustc-env=GYM_MEDIAREMOTE_TEST_CLIENT={}",
        test_client.display()
    );
}
