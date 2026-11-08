"""Check release submission archives without a Windows SDK or a Rust build."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('store_submission', ROOT / 'scripts/package-store-submission.py')
submission = importlib.util.module_from_spec(spec)
spec.loader.exec_module(submission)


class SubmissionArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='canvaz submission tests ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.packages = self.root / 'store-package'
        self.packages.mkdir()
        self.package = self.packages / 'canvaz-store-1.0.0.0-x64.msix'
        self.package.write_bytes(b'Store package fixture')
        digest = hashlib.sha256(self.package.read_bytes()).hexdigest()
        self.checksum = self.packages / (self.package.name + '.sha256')
        self.checksum.write_text(f'{digest}  {self.package.name}\n')
        self.metadata = self.packages / (self.package.name + '.build-info.json')
        self.metadata.write_text(json.dumps({'msixSha256': digest}))
        self.screenshot = self.root / 'canvaz-screenshot.png'
        self.screenshot.write_bytes(b'screenshot fixture')
        self.output = self.root / 'release-assets/store-submission.zip'

    def create_archive(self):
        return submission.create_archive(self.packages, self.screenshot, self.output)

    def test_archive_contains_package_all_artwork_and_submission_materials(self):
        # Unrelated files in the downloaded artifact must not enter the archive.
        (self.packages / 'unrelated.exe').write_bytes(b'not a submission package')
        self.create_archive()
        with zipfile.ZipFile(self.output) as archive:
            self.assertIsNone(archive.testzip())
            names = set(archive.namelist())
            for file in (self.package, self.checksum, self.metadata):
                self.assertEqual(archive.read(f'package/{file.name}'), file.read_bytes())
            self.assertEqual(archive.read('screenshots/canvaz-linux-demo.png'), self.screenshot.read_bytes())
            for directory, count in [('store-listing', 7), ('msix/Assets', 6)]:
                images = sorted((ROOT / 'packaging/windows' / directory).rglob('*.png'))
                self.assertGreaterEqual(len(images), count)
                for image in images:
                    self.assertEqual(archive.read(image.relative_to(ROOT).as_posix()), image.read_bytes())
            for name in (
                'README.md', 'APPLICATION.md', 'LICENSE',
                'packaging/windows/README.md',
                'packaging/windows/msix/store-identity.json',
                'packaging/windows/msix/AppxManifest.xml.in',
                'docs/store-packaging.md', 'docs/store-data.md',
                'docs/privacy-policy.md', 'docs/site/privacy/index.html',
            ):
                self.assertIn(name, names)
            self.assertIn(b'unsigned', archive.read('README.md'))
            self.assertNotIn('package/unrelated.exe', names)
            self.assertEqual(len(names), len(archive.namelist()))

    def test_missing_or_ambiguous_package_fails_without_creating_archive(self):
        self.package.unlink()
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            self.create_archive()
        self.package.write_bytes(b'fixture')
        (self.packages / 'second.msix').write_bytes(b'fixture')
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            self.create_archive()
        self.assertFalse(self.output.exists())

    def test_missing_checksum_or_metadata_fails_without_creating_archive(self):
        for file in (self.checksum, self.metadata):
            with self.subTest(file=file.name):
                content = file.read_bytes()
                file.unlink()
                with self.assertRaises(FileNotFoundError):
                    self.create_archive()
                self.assertFalse(self.output.exists())
                file.write_bytes(content)

    def test_corrupt_package_keeps_existing_archive(self):
        self.output.parent.mkdir()
        self.output.write_bytes(b'previous archive')
        self.package.write_bytes(b'changed package')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            self.create_archive()
        self.assertEqual(self.output.read_bytes(), b'previous archive')

    def test_mismatched_metadata_fails_without_creating_archive(self):
        self.metadata.write_text(json.dumps({'msixSha256': 'wrong digest'}))
        with self.assertRaisesRegex(ValueError, 'metadata'):
            self.create_archive()
        self.assertFalse(self.output.exists())

    def test_missing_screenshot_fails_without_creating_archive(self):
        self.screenshot.unlink()
        with self.assertRaises(FileNotFoundError):
            self.create_archive()
        self.assertFalse(self.output.exists())

    def test_store_archive_does_not_require_a_screenshot(self):
        self.screenshot.unlink()
        submission.create_archive(self.packages, None, self.output)
        with zipfile.ZipFile(self.output) as archive:
            self.assertIsNone(archive.testzip())
            self.assertIn(f'package/{self.package.name}', archive.namelist())
            self.assertIn('packaging/windows/store-listing/canvaz-app-tile-300x300.png', archive.namelist())
            self.assertFalse(any(name.startswith('screenshots/') for name in archive.namelist()))

    def test_missing_artwork_fails_without_creating_archive(self):
        with self.assertRaisesRegex(ValueError, 'Missing Store artwork'):
            submission.create_archive(self.packages, self.screenshot, self.output, root=self.root)
        self.assertFalse(self.output.exists())


if __name__ == '__main__':
    unittest.main()
