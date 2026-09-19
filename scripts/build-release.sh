#!/usr/bin/env bash
# Build Linux x64 release packages without installing them.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash scripts/build-release.sh [--bundle all|appimage|deb|rpm] [--skip-dependency-install]

Build AppImage, DEB, and RPM packages by default.
  --bundle FORMAT             Build only the selected format (default: all).
  --skip-dependency-install    Reuse node_modules instead of running npm ci.
  -h, --help                  Show this help.
EOF
}

fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
bundle=all
skip_dependencies=false
while (($#)); do
    case "$1" in
        --bundle)
            (($# >= 2)) || fail '--bundle requires a format.'
            bundle=$2
            shift 2
            ;;
        --skip-dependency-install) skip_dependencies=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "Unknown argument: $1 (use --help)." ;;
    esac
done
case "$bundle" in
    all) bundles=(appimage deb rpm) ;;
    appimage|deb|rpm) bundles=("$bundle") ;;
    *) fail 'Choose --bundle all, appimage, deb, or rpm.' ;;
esac

[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || fail 'Run this script on x86_64 Linux.'
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo_root"
for tool in node npm cargo rustc cc pkg-config; do
    command -v "$tool" >/dev/null || fail "Missing prerequisite: $tool (see README.md)."
done
rust_info=$(rustc -vV)
[[ $rust_info == *'host: x86_64-unknown-linux-gnu'* ]] || fail 'Use the native x86_64-unknown-linux-gnu Rust toolchain.'
pkg-config --exists webkit2gtk-4.1 gtk+-3.0 libudev ||
    fail 'Install WebKitGTK 4.1, GTK 3, and libudev development packages (see README.md).'
if [[ $bundle == all || $bundle == appimage ]]; then
    command -v patchelf >/dev/null || fail 'Install patchelf for AppImage packaging.'
fi

if [[ $skip_dependencies == false ]]; then
    npm ci
elif [[ ! -f node_modules/@tauri-apps/cli/tauri.js ]]; then
    fail 'Frontend dependencies are missing. Run again without --skip-dependency-install.'
fi

# Keep output predictable, independent of the caller's Cargo target directory.
export CARGO_TARGET_DIR="$repo_root/src-tauri/target"
mkdir -p -- "$CARGO_TARGET_DIR"
build_marker=$(mktemp "$CARGO_TARGET_DIR/.installer-build.XXXXXX")
trap 'rm -f -- "$build_marker"' EXIT
bundle_list=$(IFS=,; printf '%s' "${bundles[*]}")
# Rebuild the current frontend and Rust sources; never bundle a stale binary.
npm run tauri -- build --ci --no-sign --bundles "$bundle_list" -- --locked

printf '\nLocal packages built successfully:\n'
for format in "${bundles[@]}"; do
    extension=$format
    [[ $format != appimage ]] || extension=AppImage
    output_dir="$CARGO_TARGET_DIR/release/bundle/$format"
    [[ -d $output_dir ]] || fail "Missing output directory: $output_dir"
    found=false
    while IFS= read -r -d '' artifact; do
        printf '  %s\n' "$artifact"
        found=true
    done < <(find "$output_dir" -maxdepth 1 -type f -name "*.$extension" -newer "$build_marker" -print0)
    [[ $found == true ]] || fail "No new $format package found in $output_dir."
done
printf '\nPackages are unsigned local builds. No package is installed or launched automatically.\n'
