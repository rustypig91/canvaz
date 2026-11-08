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

    let git_version = if git_version.is_empty() {
        "unknown".to_string()
    } else {
        git_version
    };

    println!("cargo:rustc-env=GIT_VERSION={git_version}");
    println!("cargo:rerun-if-env-changed=CANVAZ_VERSION");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");

    // Auto-activate the platform-default backend feature so no --features flag
    // is needed at build time. Users can still override via explicit --features.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" => println!("cargo:rustc-cfg=feature=\"linux-can\""),
        _ => {}
    }

    let attributes = tauri_build::Attributes::new();
    let attributes = if target_os == "windows" && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // tauri-build 2.6 links its manifest only into app binaries. Library
        // tests also import TaskDialogIndirect, which requires Common Controls
        // v6 at process load time. Embed the same manifest in every MSVC target.
        // https://github.com/tauri-apps/tauri/issues/13419
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("windows/app.manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        attributes.windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
    } else {
        attributes
    };
    tauri_build::try_build(attributes).expect("failed to run Tauri build script");
}
