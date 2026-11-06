#!/usr/bin/env python3
"""Fill the MSIX manifest with Canvaz's Partner Center identity (stdlib only)."""

import argparse
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
MSIX = ROOT / "packaging/windows/msix"
NAMESPACES = {
    "": "http://schemas.microsoft.com/appx/manifest/foundation/windows10",
    "uap": "http://schemas.microsoft.com/appx/manifest/uap/windows10",
    "uap10": "http://schemas.microsoft.com/appx/manifest/uap/windows10/10",
    "rescap": "http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities",
}


def required_text(identity, key):
    value = identity.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(
            f"Missing {key!r}: copy Canvaz's exact value from Partner Center "
            "into packaging/windows/msix/store-identity.json. "
            "See docs/store-packaging.md."
        )
    if value != value.strip() or any(ord(char) < 32 for char in value):
        raise ValueError(f"{key!r} must not contain surrounding whitespace or control characters.")
    if re.search(r"@[A-Z_]+@", value):
        raise ValueError(f"{key!r} still contains a template placeholder.")
    return value


def package_version(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+\.0", value):
        raise ValueError("Package version must be numeric major.minor.build.0 (fourth component reserved for Store).")
    parts = [int(part) for part in value.split(".")]
    if parts[0] == 0 or any(part > 65535 for part in parts):
        raise ValueError("Package version requires a nonzero major and components at most 65535.")
    return value


def prepare(identity, version_override=None):
    if not isinstance(identity, dict):
        raise ValueError("Store identity must be a JSON object.")
    replacements = {
        "@IDENTITY_NAME@": required_text(identity, "name"),
        "@PUBLISHER@": required_text(identity, "publisher"),
        "@PUBLISHER_DISPLAY_NAME@": required_text(identity, "publisherDisplayName"),
        "@DISPLAY_NAME@": required_text(identity, "displayName"),
        "@VERSION@": package_version(version_override if version_override is not None else identity.get("packageVersion")),
    }
    tree = ET.parse(MSIX / "AppxManifest.xml.in")
    # Assign XML values rather than interpolating strings: reserved names and
    # publisher subjects may contain ampersands or quotes.
    for node in tree.iter():
        for attribute, value in list(node.attrib.items()):
            if value in replacements:
                node.set(attribute, replacements[value])
        if node.text in replacements:
            node.text = replacements[node.text]
    if re.search(rb"@[A-Z_]+@", ET.tostring(tree.getroot(), encoding="utf-8")):
        raise ValueError("Manifest contains an unresolved template placeholder.")
    return tree


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--identity", type=Path, default=MSIX / "store-identity.json")
    parser.add_argument("--package-version", help="Override the explicit Store package version")
    operation = parser.add_mutually_exclusive_group(required=True)
    operation.add_argument("--check", action="store_true", help="Validate inputs without writing a manifest")
    operation.add_argument("--output", type=Path, help="Write AppxManifest.xml to a package staging directory")
    args = parser.parse_args()
    try:
        identity = json.loads(args.identity.read_text(encoding="utf-8-sig"))
        tree = prepare(identity, args.package_version)
        if args.output:
            for prefix, namespace in NAMESPACES.items():
                ET.register_namespace(prefix, namespace)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            tree.write(args.output, encoding="utf-8", xml_declaration=True)
            print(args.output)
        else:
            print("Identity fields and package version are populated; manifest XML parses.")
    except (OSError, ValueError, ET.ParseError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
