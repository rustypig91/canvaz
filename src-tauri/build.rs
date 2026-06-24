fn main() {
    let git_version = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty=-modified"])
        .output()
        .map(|o| String::from_utf8(o.stdout).unwrap_or_default().trim().to_string())
        .unwrap_or_default();

    let git_version = if git_version.is_empty() { "unknown".to_string() } else { git_version };

    println!("cargo:rustc-env=GIT_VERSION={git_version}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");

    tauri_build::build()
}
