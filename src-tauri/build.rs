fn main() {
    // CI sets CANVAZ_VERSION to the release tag so artifacts aren't tagged
    // "-modified" just because the build step touched the working tree. Local
    // builds fall back to `git describe`, which keeps the dirty marker for dev.
    let git_version = std::env::var("CANVAZ_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("git")
                .args(["describe", "--tags", "--always", "--dirty=-modified"])
                .output()
                .map(|o| String::from_utf8(o.stdout).unwrap_or_default().trim().to_string())
                .unwrap_or_default()
        });

    let git_version = if git_version.is_empty() { "unknown".to_string() } else { git_version };

    // Drop the tag prefix so the in-app version reads "0.1.0", not "canvaz-v0.1.0".
    let git_version = git_version
        .strip_prefix("canvaz-v")
        .map(str::to_string)
        .unwrap_or(git_version);

    println!("cargo:rustc-env=GIT_VERSION={git_version}");
    println!("cargo:rerun-if-env-changed=CANVAZ_VERSION");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");

    // Auto-activate the platform-default backend feature so no --features flag
    // is needed at build time. Users can still override via explicit --features.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => println!("cargo:rustc-cfg=feature=\"kvaser\""),
        "linux"   => println!("cargo:rustc-cfg=feature=\"linux-can\""),
        _ => {}
    }

    tauri_build::build()
}
