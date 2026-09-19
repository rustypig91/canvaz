//! GitHub release installation, using the same checksum policy as Pigtail.
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tauri::{Emitter, Manager};
const REPO: &str = "rustypig91/canvaz";
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
#[derive(Default)]
pub struct UpdateState(Mutex<Option<Prepared>>);
struct Prepared {
    directory: tempfile::TempDir,
    file: PathBuf,
    target: PathBuf,
}
struct Asset {
    url: String,
    size: u64,
    digest: String,
}

fn destination() -> Result<PathBuf, String> {
    if cfg!(windows) {
        return std::env::current_exe().map_err(|e| e.to_string());
    }
    if cfg!(target_os = "linux") {
        if let Some(path) = std::env::var_os("APPIMAGE") {
            let path = PathBuf::from(path);
            if path.is_absolute() && path.is_file() {
                return Ok(path);
            }
        }
        return Err("Use your package manager to install the latest .deb or .rpm. In-app updates require an AppImage.".into());
    }
    Err("Automatic installation is not available for this platform.".into())
}
#[tauri::command]
pub fn update_support() -> Result<(), String> {
    if std::env::consts::ARCH != "x86_64" {
        return Err("Automatic updates are not available for this architecture.".into());
    }
    destination().map(|_| ())
}

fn select_asset(json: &serde_json::Value, version: &str, nsis: bool) -> Result<Asset, String> {
    let suffix = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") if nsis => "_x64-setup.exe",
        ("windows", "x86_64") => "_x64_en-US.msi",
        ("linux", "x86_64") => "_amd64.AppImage",
        _ => return Err("No update artifact is available for this platform.".into()),
    };
    let assets = json["assets"].as_array().ok_or("Missing release assets")?;
    let matches: Vec<_> = assets
        .iter()
        .filter(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
        .collect();
    if matches.len() != 1 {
        return Err("The update artifact is not available yet. Try again after publishing finishes.".into());
    }
    let asset = matches[0];
    let name = asset["name"].as_str().unwrap();
    let url = asset["browser_download_url"].as_str().unwrap_or_default();
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let decoded = percent_encoding::percent_decode_str(parsed.path())
        .decode_utf8()
        .map_err(|e| e.to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || decoded != format!("/{REPO}/releases/download/{version}/{name}")
    {
        return Err("Unexpected update download address.".into());
    }
    let size = asset["size"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= MAX_DOWNLOAD)
        .ok_or("Invalid update size")?;
    let digest = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("The release has no SHA-256 checksum; automatic installation is unavailable.")?;
    Ok(Asset {
        url: url.into(),
        size,
        digest: digest.to_ascii_lowercase(),
    })
}

#[tauri::command]
pub async fn download_update(app: tauri::AppHandle, version: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<UpdateState>();
        let mut pending = state.0.try_lock().map_err(|_| "An update is already in progress")?;
        *pending = None;
        let target = destination()?;
        if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)) {
            return Err("Invalid release version".into());
        }
        let body = ureq::get(&format!("https://api.github.com/repos/{REPO}/releases/tags/{version}"))
            .header("User-Agent", "canvaz-updater")
            .config()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .call()
            .map_err(|e| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let json = serde_json::from_str(&body).map_err(|e| format!("Invalid release metadata: {e}"))?;
        let nsis = target.parent().is_some_and(|p| p.join("uninstall.exe").is_file());
        let asset = select_asset(&json, &version, nsis)?;
        let directory = tempfile::Builder::new()
            .prefix("canvaz-update-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let file = directory.path().join(if cfg!(windows) {
            if nsis {
                "setup.exe"
            } else {
                "update.msi"
            }
        } else {
            "update.AppImage"
        });
        let mut output = std::fs::File::create(&file).map_err(|e| e.to_string())?;
        let mut response = ureq::get(&asset.url)
            .config()
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .call()
            .map_err(|e| e.to_string())?;
        let mut last = 0;
        copy_verified(response.body_mut().as_reader(), &mut output, &asset, |downloaded| {
            if downloaded - last >= 256 * 1024 || downloaded == asset.size {
                last = downloaded;
                let _ = app.emit("update-progress", downloaded as f64 / asset.size as f64);
            }
        })?;
        output.sync_all().map_err(|e| e.to_string())?;
        *pending = Some(Prepared { directory, file, target });
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let prepared = app
            .state::<UpdateState>()
            .0
            .try_lock()
            .map_err(|_| "An update is already in progress")?
            .take()
            .ok_or("Download the update first")?;
        install(prepared)?;
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(windows)]
fn install(update: Prepared) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let script = setup_script(&update)?;
    let helper = update.directory.path().join("install.ps1");
    std::fs::write(&helper, format!("\u{feff}{script}")).map_err(|e| e.to_string())?;
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(helper)
        .creation_flags(0x08000000)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let _ = update.directory.keep();
    Ok(())
}
#[cfg(windows)]
fn setup_script(update: &Prepared) -> Result<String, String> {
    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "''"))
    }
    let dir = update.target.parent().ok_or("Missing installation folder")?;
    Ok(format!(
        r#"$ErrorActionPreference = 'Stop'
$staging = {staging}
try {{
    $parent = Get-Process -Id {pid} -ErrorAction SilentlyContinue
    if ($parent -and -not $parent.WaitForExit(120000)) {{ throw 'Canvaz did not close in time.' }}
    if ([IO.Path]::GetExtension({file}) -eq '.msi') {{
        $process = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/i', ('"'+{file}+'"'), '/passive', '/norestart', ('INSTALLDIR="'+{dir}+'"')) -Wait -PassThru
    }} else {{
        # Match the existing installation, not the helper's elevation level.
        function Normalize-InstallPath($path) {{
            if (-not $path) {{ return '' }}
            return [IO.Path]::GetFullPath($path.Trim('"')).TrimEnd('\')
        }}
        $scope = $null
        $destination = Normalize-InstallPath {dir}
        foreach ($entry in @(@('HKCU:', '/CurrentUser'), @('HKLM:', '/AllUsers'))) {{
            $key = Get-ItemProperty -LiteralPath ($entry[0] + '\Software\Microsoft\Windows\CurrentVersion\Uninstall\Rusty''s Canvaz - CAN Analyzer') -ErrorAction SilentlyContinue
            if ($key -and (Normalize-InstallPath $key.InstallLocation) -eq $destination) {{
                if ($scope) {{ throw 'Both installation scopes use this folder. Please update using setup.exe.' }}
                $scope = $entry[1]
            }}
        }}
        if (-not $scope) {{ throw 'Could not determine the installation scope. Please update using setup.exe.' }}
        # NSIS requires /D to be last and its path must not be quoted.
        $process = Start-Process -FilePath {file} -ArgumentList @('/P','/UPDATE',$scope,('/D='+{dir})) -Wait -PassThru
    }}
    if ($process.ExitCode -notin @(0, 3010)) {{ throw "Installer exited with code $($process.ExitCode)." }}
}} catch {{
    Add-Type -AssemblyName System.Windows.Forms
    [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Canvaz update failed') | Out-Null
}} finally {{
    if (-not (Get-Process -Id {pid} -ErrorAction SilentlyContinue)) {{ Start-Process -FilePath {target} -WorkingDirectory {dir} }}
    $resolved = [IO.Path]::GetFullPath($staging)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and [IO.Path]::GetFileName($resolved).StartsWith('canvaz-update-')) {{
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction SilentlyContinue
    }}
}}
"#,
        staging = quote(update.directory.path()),
        file = quote(&update.file),
        target = quote(&update.target),
        dir = quote(dir),
        pid = std::process::id()
    ))
}
#[cfg(not(windows))]
fn install(update: Prepared) -> Result<(), String> {
    let _ = &update.directory;
    replace_appimage(&update.file, &update.target)?;
    restart_after_exit(&update.target, std::process::id())?;
    Ok(())
}

#[cfg(unix)]
fn restart_after_exit(target: &Path, pid: u32) -> Result<std::process::Child, String> {
    // The new instance auto-opens CAN channels. Wait until this process releases
    // its devices before starting it. Pass paths as arguments, never shell code.
    std::process::Command::new("/bin/sh")
        .args(["-c", r#"while kill -0 "$1" 2>/dev/null; do sleep 0.1; done
exec "$2""#, "canvaz-restart"])
        .arg(pid.to_string())
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Updated, but could not schedule restart: {e}"))
}

fn copy_verified(mut reader: impl Read, mut writer: impl Write, asset: &Asset, mut progress: impl FnMut(u64)) -> Result<(), String> {
    let mut hash = Sha256::new();
    let mut downloaded = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|e| format!("Download interrupted: {e}"))?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        if downloaded > asset.size {
            return Err("The download is larger than the published file.".into());
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|e| format!("Could not save the update: {e}"))?;
        hash.update(&buffer[..count]);
        progress(downloaded);
    }
    if downloaded != asset.size {
        return Err("The download was incomplete. Please try again.".into());
    }
    if format!("{:x}", hash.finalize()) != asset.digest {
        return Err("The update checksum did not match. Please try again.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn restart_waits_for_previous_process_to_exit() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("Canvaz's updated app");
        std::fs::write(&target, "#!/bin/sh\nexit 42\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut parent = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut helper = restart_after_exit(&target, parent.id()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(helper.try_wait().unwrap().is_none());
        parent.kill().unwrap();
        parent.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert_eq!(status.code(), Some(42));
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = helper.kill();
                panic!("Restart helper did not launch the updated application");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_parses_paths_with_spaces_quotes_and_unicode() {
        use std::os::windows::process::CommandExt;
        let directory = tempfile::tempdir().unwrap();
        let update = Prepared {
            file: directory.path().join("setup.exe"),
            target: PathBuf::from("C:\\Users\\O'Brien å\\Canvaz\\canvaz.exe"),
            directory,
        };
        let script = setup_script(&update).unwrap();
        assert!(script.contains("O''Brien å"));
        let helper = update.directory.path().join("syntax.ps1");
        std::fs::write(&helper, format!("\u{feff}{script}")).unwrap();
        // Parse only: this test never launches an installer or modifies an installation.
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$tokens=$null; $errors=$null; [void][System.Management.Automation.Language.Parser]::ParseFile($env:CANVAZ_SCRIPT_TEST, [ref]$tokens, [ref]$errors); if ($errors.Count) { $errors | Out-String | Write-Error; exit 1 }"])
            .env("CANVAZ_SCRIPT_TEST", &helper)
            .creation_flags(0x08000000)
            .status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn download_requires_exact_size_and_checksum() {
        let data = b"verified update";
        let mut asset = Asset {
            url: String::new(),
            size: data.len() as u64,
            digest: format!("{:x}", Sha256::digest(data)),
        };
        let mut output = Vec::new();
        copy_verified(&data[..], &mut output, &asset, |_| {}).unwrap();
        assert_eq!(output, data);
        assert!(copy_verified(&data[..3], Vec::new(), &asset, |_| {}).is_err());
        asset.size -= 1;
        assert!(copy_verified(&data[..], Vec::new(), &asset, |_| {}).is_err());
        asset.size += 1;
        asset.digest = "0".repeat(64);
        assert!(copy_verified(&data[..], Vec::new(), &asset, |_| {}).is_err());
    }

    #[test]
    fn release_asset_requires_trusted_url_and_digest() {
        let suffix = if cfg!(windows) { "_x64-setup.exe" } else { "_amd64.AppImage" };
        let name = format!("Rusty's Canvaz_0.5.0{suffix}");
        let mut json = serde_json::json!({ "assets": [{
            "name": name, "size": 100, "digest": format!("sha256:{}", "a".repeat(64)),
            "browser_download_url": format!("https://github.com/{REPO}/releases/download/v0.5.0/{}", name.replace(' ', "%20").replace('\'', "%27"))
        }] });
        assert!(select_asset(&json, "v0.5.0", true).is_ok());
        assert!(select_asset(&json, "v0.4.0", true).is_err());
        json["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(select_asset(&json, "v0.5.0", true).is_err());
        json["assets"][0]["digest"] = format!("sha256:{}", "a".repeat(64)).into();
        json["assets"][0]["browser_download_url"] = "https://example.com/setup.exe".into();
        assert!(select_asset(&json, "v0.5.0", true).is_err());
    }
}

#[cfg(all(unix, not(windows)))]
fn replace_appimage(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let parent = target.parent().ok_or("Could not locate the AppImage folder.")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|e| format!("The AppImage folder is not writable: {e}"))?;
    std::io::copy(&mut std::fs::File::open(source).map_err(|e| e.to_string())?, &mut staged).map_err(|e| e.to_string())?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    staged.as_file().sync_all().map_err(|e| e.to_string())?;
    staged.persist(target).map_err(|e| format!("Could not replace the AppImage: {e}"))?;
    Ok(())
}
