#!/usr/bin/env python3
"""Archive a built Store package and its listing materials for a GitHub release."""

import argparse
import hashlib
import json
from pathlib import Path
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parents[1]


def create_archive(package_directory, screenshot, output, root=ROOT):
    packages = sorted(package_directory.glob('*.msix'))
    if len(packages) != 1:
        raise ValueError('Expected exactly one Store MSIX package')
    package = packages[0]
    checksum = package.with_name(package.name + '.sha256')
    metadata = package.with_name(package.name + '.build-info.json')
    with package.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    if checksum.read_text(encoding='utf-8').split()[0] != digest:
        raise ValueError('Store package checksum does not match the MSIX')
    if json.loads(metadata.read_text(encoding='utf-8'))['msixSha256'] != digest:
        raise ValueError('Store build metadata does not match the MSIX')

    files = {
        'README.md': root / 'packaging/windows/store-submission-README.md',
        'APPLICATION.md': root / 'README.md',
        'LICENSE': root / 'LICENSE',
        'screenshots/canvaz-linux-demo.png': screenshot,
        **{f'package/{file.name}': file for file in (package, checksum, metadata)},
    }
    for relative in (
        'packaging/windows/README.md',
        'packaging/windows/msix/store-identity.json',
        'packaging/windows/msix/AppxManifest.xml.in',
        'docs/store-packaging.md',
        'docs/store-data.md',
        'docs/privacy-policy.md',
        'docs/site/privacy/index.html',
    ):
        files[relative] = root / relative
    for directory in ('packaging/windows/store-listing', 'packaging/windows/msix/Assets'):
        images = sorted((root / directory).rglob('*.png'))
        if not images:
            raise ValueError(f'Missing Store artwork: {directory}')
        for image in images:
            files[image.relative_to(root).as_posix()] = image
    for file in files.values():
        if not file.is_file():
            raise FileNotFoundError(file)

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent, suffix='.zip.partial', delete=False) as pending:
        temporary = Path(pending.name)
    try:
        with zipfile.ZipFile(temporary, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, file in sorted(files.items()):
                archive.write(file, name)
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package-directory', type=Path, required=True)
    parser.add_argument('--screenshot', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    print(create_archive(args.package_directory, args.screenshot, args.output))


if __name__ == '__main__':
    main()
