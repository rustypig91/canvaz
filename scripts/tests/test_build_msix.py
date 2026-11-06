"""Exercise Store packaging contracts without Rust builds or a Windows SDK."""

import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("build_msix", ROOT / "scripts/build-msix.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


def pe_file(path, machine=0x8664):
    path.parent.mkdir(parents=True, exist_ok=True)
    content = bytearray(128)
    content[:2] = b"MZ"
    struct.pack_into("<I", content, 0x3C, 64)
    content[64:70] = b"PE\0\0" + struct.pack("<H", machine)
    path.write_bytes(content)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="canvaz msix tests ")
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/prepare-store-manifest.py", self.root / "scripts")
        shutil.copytree(ROOT / "packaging/windows/msix", self.root / "packaging/windows/msix")
        # Keep the fixture version stable when the real Store version advances.
        identity_path = self.root / "packaging/windows/msix/store-identity.json"
        identity = json.loads(identity_path.read_text())
        identity["packageVersion"] = "1.0.0.0"
        identity_path.write_text(json.dumps(identity))
        (self.root / "LICENSE").write_text("license fixture")
        # Deliberately leave a stale direct-download executable outside Store.
        pe_file(self.root / "src-tauri/target/release/canvaz.exe", 0x14C)
        self.calls = []
        self.fail = None
        self.demo = False
        self.custom_protocol = True
        self.package_args = argparse.Namespace(package_version=None, output_directory="output with spaces", skip_dependency_install=False)

    def tearDown(self):
        self.temp.cleanup()

    def fake_run(self, arguments, env, capture=False):
        args = list(map(str, arguments))
        self.calls.append(args)
        self.assertIn("-Ctarget-feature=+crt-static", env["CARGO_ENCODED_RUSTFLAGS"])
        self.assertNotIn("TAURI_CONFIG", env)
        if args[:2] == ["cargo", "build"]:
            self.assertIn("--no-default-features", args)
            self.assertIn(builder.FEATURES, args)
            pe_file(Path(env["CARGO_TARGET_DIR"]) / builder.TARGET / "release/canvaz.exe")
        elif args[:2] == ["cargo", "metadata"]:
            self.assertTrue(capture)
            return json.dumps({
                "packages": [{"id": "app", "name": "canvaz", "version": "0.6.4"}, {"id": "tauri", "name": "tauri"}],
                "resolve": {"root": "app", "nodes": [
                    {"id": "app", "features": ["microsoft-store", "kvaser", "pcan"] + (["demo"] if self.demo else [])},
                    {"id": "tauri", "features": ["custom-protocol"] if self.custom_protocol else []},
                ]},
            })
        elif args[0] == "expand.exe":
            runtime = Path(args[-1]) / "Microsoft.WebView2.FixedVersionRuntime.fixture.x64"
            pe_file(runtime / "msedgewebview2.exe")
            (runtime / "runtime-license.txt").write_text("runtime license fixture")
        elif args[:2] == ["MakePri.exe", "createconfig"]:
            Path(args[args.index("/cf") + 1]).write_text("<resources><packaging><autoResourcePackage/></packaging></resources>")
        elif args[:2] == ["MakePri.exe", "new"]:
            config = Path(args[args.index("/cf") + 1]).read_text()
            self.assertNotIn("packaging", config)
            Path(args[args.index("/of") + 1]).write_bytes(b"PRI fixture")
        elif args[:2] == ["MakeAppx.exe", "pack"]:
            stage = Path(args[args.index("/d") + 1])
            builder.require_x64_exe(stage / "canvaz.exe")
            builder.require_x64_exe(stage / "WebView2/msedgewebview2.exe")
            self.assertTrue((stage / "WebView2/runtime-license.txt").is_file())
            self.assertTrue((stage / "Assets/Square44x44Logo.scale-200.png").is_file())
            self.assertTrue((stage / "resources.pri").is_file())
            self.assertNotIn("/nv", args)  # Never disable MakeAppx validation.
            if self.fail:
                raise subprocess.CalledProcessError(1, args)
            Path(args[args.index("/p") + 1]).write_bytes(b"MSIX fixture")
        return None

    def build(self):
        with patch.object(builder, "ROOT", self.root), patch.object(builder.sys, "platform", "win32"), \
                patch.object(builder, "sdk_tool", side_effect=lambda name: name), \
                patch.object(builder, "checked_download", return_value=self.root / "runtime.cab"), \
                patch.object(builder, "run", side_effect=self.fake_run):
            return builder.build(self.package_args)

    def test_success_stages_only_store_outputs_with_runtime_and_metadata(self):
        before_env, before_cwd = dict(os.environ), Path.cwd()
        self.build()
        output = self.root / self.package_args.output_directory
        package = output / "canvaz-store-1.0.0.0-x64.msix"
        self.assertEqual(package.read_bytes(), b"MSIX fixture")
        info = json.loads((output / f"{package.name}.build-info.json").read_text())
        self.assertEqual(info["applicationVersion"], "0.6.4")
        self.assertEqual(info["packageVersion"], "1.0.0.0")
        self.assertEqual(info["storeId"], "9P3PGB2ZZ67L")
        self.assertEqual(info["msixSha256"], hashlib.sha256(b"MSIX fixture").hexdigest())
        self.assertTrue((output / f"{package.name}.sha256").read_text().startswith(info["msixSha256"]))
        self.assertFalse(list((self.root / "src-tauri/target/store").glob("staging-*")))
        self.assertEqual(dict(os.environ), before_env)
        self.assertEqual(Path.cwd(), before_cwd)
        self.assertIn(["npm.cmd", "ci"], self.calls)
        self.assertIn(["npm.cmd", "run", "build"], self.calls)

    def test_failure_cleans_staging_and_preserves_existing_artifact_and_environment(self):
        self.fail = True
        output = self.root / self.package_args.output_directory
        output.mkdir()
        package = output / "canvaz-store-1.0.0.0-x64.msix"
        package.write_bytes(b"previous successful package")
        before_env, before_cwd = dict(os.environ), Path.cwd()
        with self.assertRaises(subprocess.CalledProcessError):
            self.build()
        self.assertEqual(package.read_bytes(), b"previous successful package")
        self.assertFalse(list((self.root / "src-tauri/target/store").glob("staging-*")))
        self.assertEqual(dict(os.environ), before_env)
        self.assertEqual(Path.cwd(), before_cwd)

    def test_demo_feature_is_rejected_before_packaging(self):
        self.demo = True
        with self.assertRaisesRegex(ValueError, "Refusing to package"):
            self.build()
        self.assertFalse(any(call[:2] == ["MakeAppx.exe", "pack"] for call in self.calls))
        self.assertFalse((self.root / self.package_args.output_directory).exists())

    def test_explicit_version_and_dependency_install_skip(self):
        self.package_args.package_version = "1.0.2.0"
        self.package_args.skip_dependency_install = True
        self.build()
        self.assertNotIn(["npm.cmd", "ci"], self.calls)
        self.assertIn(["npm.cmd", "run", "build"], self.calls)
        self.assertTrue((self.root / self.package_args.output_directory / "canvaz-store-1.0.2.0-x64.msix").is_file())

    def test_development_server_build_is_rejected_before_packaging(self):
        self.custom_protocol = False
        with self.assertRaisesRegex(ValueError, "Refusing to package"):
            self.build()
        self.assertFalse(any(call[:2] == ["MakeAppx.exe", "pack"] for call in self.calls))

    def test_invalid_version_stops_before_external_tools(self):
        self.package_args.package_version = "0.6.4.0"
        with self.assertRaisesRegex(ValueError, "nonzero major"):
            self.build()
        self.assertFalse(self.calls)


class DependencyTests(unittest.TestCase):
    def test_captured_cargo_json_uses_utf8_instead_of_the_windows_code_page(self):
        metadata = {"description": "Metadata with Unicode: \u0101 \u65e5\u672c\u8a9e"}
        payload = json.dumps(metadata, ensure_ascii=False).encode("utf-8")
        with patch.object(builder.subprocess, "run", wraps=subprocess.run) as command:
            captured = builder.run(
                [sys.executable, "-c", f"import sys; sys.stdout.buffer.write({payload!r})"],
                dict(os.environ), capture=True,
            )
        self.assertEqual(json.loads(captured), metadata)
        self.assertEqual(command.call_args.kwargs["encoding"], "utf-8")

    def test_flags_preserve_encoded_precedence_and_do_not_mutate_environment(self):
        for encoded, plain, expected in [
            (None, "  --cfg=plain \t -C debuginfo=0 ", "--cfg=plain\x1f-C\x1fdebuginfo=0\x1f"),
            (None, " \t ", ""),
            ("", "--cfg=ignored", ""),
            ("--cfg=encoded\x1f-C\x1fdebuginfo=0", "--cfg=ignored", "--cfg=encoded\x1f-C\x1fdebuginfo=0\x1f"),
        ]:
            with self.subTest(encoded=encoded, plain=plain), patch.dict(os.environ, {"RUSTFLAGS": plain, "TAURI_CONFIG": "ignored"}, clear=True):
                if encoded is not None:
                    os.environ["CARGO_ENCODED_RUSTFLAGS"] = encoded
                before = dict(os.environ)
                result = builder.build_environment()
                self.assertEqual(result["CARGO_ENCODED_RUSTFLAGS"], expected + "-Ctarget-feature=+crt-static")
                self.assertNotIn("TAURI_CONFIG", result)
                self.assertEqual(dict(os.environ), before)

    def test_sdk_lookup_sorts_versions_numerically(self):
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"ProgramFiles(x86)": temp}), patch.object(builder.shutil, "which", return_value=None):
            for version in ["10.0.9999.0", "10.0.26100.0"]:
                exe = Path(temp) / "Windows Kits/10/bin" / version / "x64/MakeAppx.exe"
                exe.parent.mkdir(parents=True)
                exe.touch()
            self.assertIn("10.0.26100.0", str(builder.sdk_tool("MakeAppx.exe")))

    def test_pe_check_rejects_non_windows_and_wrong_architecture(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "application.exe"
            pe_file(path)
            builder.require_x64_exe(path)
            pe_file(path, 0x14C)
            with self.assertRaisesRegex(ValueError, "Not an x64"):
                builder.require_x64_exe(path)
            path.write_bytes(b"not windows")
            with self.assertRaisesRegex(ValueError, "Not a Windows"):
                builder.require_x64_exe(path)

    def test_download_rejects_tampering_and_reuses_only_verified_cache(self):
        data = b"fixture runtime CAB"
        runtime = json.loads((ROOT / "packaging/windows/msix/webview2-runtime.json").read_text())
        runtime["sha256"] = hashlib.sha256(data).hexdigest()
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            with patch.object(builder.urllib.request, "urlopen", return_value=io.BytesIO(b"tampered")):
                with self.assertRaisesRegex(ValueError, "checksum"):
                    builder.checked_download(runtime, cache)
            self.assertEqual(list(cache.iterdir()), [])
            with patch.object(builder.urllib.request, "urlopen", return_value=io.BytesIO(data)) as request:
                downloaded = builder.checked_download(runtime, cache)
                self.assertEqual(downloaded.read_bytes(), data)
                builder.checked_download(runtime, cache)
                self.assertEqual(request.call_count, 1)

    def test_runtime_descriptor_rejects_untrusted_or_wrong_architecture_sources(self):
        runtime = json.loads((ROOT / "packaging/windows/msix/webview2-runtime.json").read_text())
        for invalid in [dict(runtime, architecture="x86"), dict(runtime, url="http://example.com/runtime.cab"), dict(runtime, sha256="invalid")]:
            with self.subTest(invalid=invalid), tempfile.TemporaryDirectory() as temp, self.assertRaisesRegex(ValueError, "Invalid pinned"):
                builder.checked_download(invalid, Path(temp))


if __name__ == "__main__":
    unittest.main()
