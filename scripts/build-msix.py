#!/usr/bin/env python3
"""Build an unsigned x64 Store MSIX on Windows with Rust, Node, Python and the SDK."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"
FEATURES = "microsoft-store,kvaser,pcan,tauri/custom-protocol"
RUNTIME_FOLDER = "WebView2"


def run(arguments, env, capture=False):
    result = subprocess.run(
        [str(arg) for arg in arguments], cwd=ROOT, env=env, check=True,
        stdout=subprocess.PIPE if capture else None,
        # Cargo's JSON is UTF-8 even when Windows uses a legacy code page.
        # Leave inherited console output alone for SDK tools and build logs.
        encoding="utf-8" if capture else None,
    )
    return result.stdout if capture else None


def build_environment():
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(ROOT / "src-tauri/target/store")
    # Encoded flags take precedence, including an explicitly empty value.
    flags = env.get("CARGO_ENCODED_RUSTFLAGS")
    if flags is None:
        flags = "\x1f".join(env.get("RUSTFLAGS", "").split())
    env["CARGO_ENCODED_RUSTFLAGS"] = (flags + "\x1f" if flags else "") + "-Ctarget-feature=+crt-static"
    # A caller's development overlay must not change the Store build's assets,
    # app identifier, or dev URL. The checked-in config is the build contract.
    env.pop("TAURI_CONFIG", None)
    return env


def sdk_tool(name):
    found = shutil.which(name)
    if found:
        return Path(found)
    sdk = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Windows Kits/10/bin"
    versions = [p for p in sdk.glob("10.*") if re.fullmatch(r"10\.[0-9]+\.[0-9]+\.[0-9]+", p.name)]
    for version in sorted(versions, key=lambda p: tuple(map(int, p.name.split("."))), reverse=True):
        candidate = version / "x64" / name
        if candidate.is_file():
            return candidate
    raise ValueError(f"Install the Windows 10/11 SDK, including {name}.")


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def checked_download(runtime, cache):
    url = urllib.parse.urlparse(runtime["url"])
    version = runtime["version"]
    if (url.scheme != "https" or url.hostname != "msedge.sf.dl.delivery.mp.microsoft.com"
            or runtime["architecture"] != "x64"
            or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+", version)
            or not url.path.endswith(f"Microsoft.WebView2.FixedVersionRuntime.{version}.x64.cab")
            or not re.fullmatch(r"[a-fA-F0-9]{64}", runtime["sha256"])):
        raise ValueError("Invalid pinned x64 WebView2 runtime metadata.")
    expected = runtime["sha256"].lower()
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / f"WebView2-{version}-x64.cab"
    if destination.is_file() and sha256(destination) == expected:
        return destination
    with tempfile.NamedTemporaryFile(dir=cache, suffix=".partial", delete=False) as output:
        temporary = Path(output.name)
        try:
            print(f"Downloading Microsoft WebView2 {version} (x64)...", flush=True)
            with urllib.request.urlopen(runtime["url"], timeout=60) as response:
                shutil.copyfileobj(response, output)
        except BaseException:
            output.close()
            temporary.unlink(missing_ok=True)
            raise
    try:
        if sha256(temporary) != expected:
            raise ValueError("WebView2 CAB checksum does not match the pinned SHA-256.")
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def require_x64_exe(path):
    with path.open("rb") as binary:
        header = binary.read(64)
        if len(header) != 64 or header[:2] != b"MZ":
            raise ValueError(f"Not a Windows executable: {path}")
        offset = struct.unpack_from("<I", header, 0x3C)[0]
        binary.seek(offset)
        if binary.read(6) != b"PE\0\0\x64\x86":
            raise ValueError(f"Not an x64 Windows executable: {path}")


def manifest_module():
    spec = importlib.util.spec_from_file_location("store_manifest", ROOT / "scripts/prepare-store-manifest.py")
    module = importlib.util.module_from_spec(spec)
    # Avoid writing __pycache__ into the checkout just to load this helper.
    previous = sys.dont_write_bytecode
    try:
        sys.dont_write_bytecode = True
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = previous
    return module


def build(args):
    if sys.platform != "win32":
        raise ValueError("Run this build on Windows with the x64 MSVC toolchain and Windows SDK.")
    env = build_environment()
    helper = manifest_module()
    # Accept Windows PowerShell's UTF-8 BOM, as the manifest checker does.
    identity = json.loads((ROOT / "packaging/windows/msix/store-identity.json").read_text(encoding="utf-8-sig"))
    manifest = helper.prepare(identity, args.package_version)
    version = manifest.getroot().find(f"{{{helper.NAMESPACES['']}}}Identity").get("Version")
    runtime = json.loads((ROOT / "packaging/windows/msix/webview2-runtime.json").read_text(encoding="utf-8-sig"))
    makeappx, makepri = sdk_tool("MakeAppx.exe"), sdk_tool("MakePri.exe")
    output = Path(args.output_directory)
    if not output.is_absolute():
        output = ROOT / output
    output = output.resolve()
    target_dir = Path(env["CARGO_TARGET_DIR"])
    target_dir.mkdir(parents=True, exist_ok=True)

    # Use the production custom protocol (embedded frontend), never the dev URL.
    # Explicit features and --no-default-features prevent a demo/default-feature
    # change from silently turning a screenshot binary into a Store submission.
    rust_flags = ["--manifest-path", ROOT / "src-tauri/Cargo.toml", "--locked",
                  "--target", TARGET, "--no-default-features", "--features", FEATURES]
    run(["rustup", "target", "add", TARGET], env)
    if not args.skip_dependency_install:
        run(["npm.cmd", "ci"], env)
    run(["npm.cmd", "run", "build"], env)
    run(["cargo", "build", "--release", *rust_flags], env)
    metadata = json.loads(run([
        "cargo", "metadata", "--manifest-path", ROOT / "src-tauri/Cargo.toml", "--locked",
        "--format-version", "1", "--filter-platform", TARGET, "--no-default-features", "--features", FEATURES,
    ], env, capture=True))
    app = next(p for p in metadata["packages"] if p["id"] == metadata["resolve"]["root"])
    active = next(n["features"] for n in metadata["resolve"]["nodes"] if n["id"] == app["id"])
    tauri_id = next(p["id"] for p in metadata["packages"] if p["name"] == "tauri")
    tauri_features = next(n["features"] for n in metadata["resolve"]["nodes"] if n["id"] == tauri_id)
    if "demo" in active or not {"microsoft-store", "kvaser", "pcan"}.issubset(active) or "custom-protocol" not in tauri_features:
        raise ValueError("Refusing to package a build without the production Store/CAN features or embedded frontend.")

    binary_dir = target_dir / TARGET / "release"
    require_x64_exe(binary_dir / "canvaz.exe")
    cab = checked_download(runtime, ROOT / "src-tauri/target/store-downloads")
    # Temporary staging is removed after successful packaging and every failure.
    # Existing target/release or demo files never enter this package.
    with tempfile.TemporaryDirectory(prefix="staging-", dir=target_dir) as work:
        work = Path(work)
        stage = work / "layout"
        stage.mkdir()
        extracted = work / "runtime"
        extracted.mkdir()
        run(["expand.exe", str(cab), "-F:*", str(extracted)], env)
        runtimes = list(extracted.rglob("msedgewebview2.exe"))
        if len(runtimes) != 1:
            raise ValueError("Expected exactly one WebView2 runtime executable in the pinned CAB.")
        require_x64_exe(runtimes[0])
        shutil.copytree(runtimes[0].parent, stage / RUNTIME_FOLDER)
        shutil.copy2(binary_dir / "canvaz.exe", stage / "canvaz.exe")
        # Some WebView2 loader configurations use a companion DLL; copy it if
        # emitted. The usual Rust WebView2 loader is statically linked.
        if (binary_dir / "WebView2Loader.dll").is_file():
            shutil.copy2(binary_dir / "WebView2Loader.dll", stage)
        shutil.copy2(ROOT / "LICENSE", stage)
        shutil.copytree(ROOT / "packaging/windows/msix/Assets", stage / "Assets")
        for prefix, namespace in helper.NAMESPACES.items():
            ET.register_namespace(prefix, namespace)
        manifest.write(stage / "AppxManifest.xml", encoding="utf-8", xml_declaration=True)

        pri_config = work / "priconfig.xml"
        run([makepri, "createconfig", "/cf", pri_config, "/dq", "en-US", "/pv", "10.0.0", "/o"], env)
        pri = ET.parse(pri_config)
        # Keep one self-contained MSIX instead of separate resource packages.
        for packaging in pri.getroot().findall("packaging"):
            pri.getroot().remove(packaging)
        pri.write(pri_config, encoding="utf-8", xml_declaration=True)
        run([makepri, "new", "/pr", stage, "/cf", pri_config, "/of", stage / "resources.pri", "/o"], env)
        package_name = f"canvaz-store-{version}-x64.msix"
        temporary_package = work / package_name
        run([makeappx, "pack", "/o", "/h", "SHA256", "/d", stage, "/p", temporary_package], env)
        if not temporary_package.is_file():
            raise ValueError("MakeAppx did not produce the submission package.")
        output.mkdir(parents=True, exist_ok=True)
        # Replace final output only after a successful, validated SDK build.
        destination = output / package_name
        with tempfile.NamedTemporaryFile(dir=output, suffix=".msix.partial", delete=False) as copied:
            pending_output = Path(copied.name)
        try:
            shutil.copy2(temporary_package, pending_output)
            digest = sha256(pending_output)
            pending_output.replace(destination)
        finally:
            pending_output.unlink(missing_ok=True)
        (output / f"{package_name}.sha256").write_text(f"{digest}  {package_name}\n", encoding="utf-8")
        info = {
            "applicationVersion": app["version"], "packageVersion": version,
            "identity": identity["name"], "storeId": identity.get("storeId"),
            "target": TARGET, "features": sorted(active), "webview2": runtime,
            "msixSha256": digest,
        }
        (output / f"{package_name}.build-info.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
    print(f"Store submission package (unsigned): {destination}")
    print("Upload to Partner Center; Microsoft signs the MSIX after certification.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-version", help="Override the explicit Store package version")
    parser.add_argument("--output-directory", default="src-tauri/target/release-assets/store")
    parser.add_argument("--skip-dependency-install", action="store_true", help="Reuse an existing npm dependency installation")
    args = parser.parse_args()
    try:
        build(args)
    except (OSError, ValueError, KeyError, StopIteration, ET.ParseError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Store build failed: {error}\n")


if __name__ == "__main__":
    main()
