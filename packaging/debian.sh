#!/usr/bin/env bash
# Retain Tauri's generated resources/dependencies; normalize the Debian identity.
set -euo pipefail
input=$1
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dpkg-deb -R "$input" "$stage"
old=$(dpkg-deb -f "$input" Package)
version=$(dpkg-deb -f "$input" Version)
arch=$(dpkg-deb -f "$input" Architecture)
dpkg --validate-version "$version"
sed -i 's/^Package: .*/Package: canvaz/' "$stage/DEBIAN/control"
if [[ "$old" != canvaz ]]; then
  printf 'Conflicts: %s\nReplaces: %s\n' "$old" "$old" >> "$stage/DEBIAN/control"
fi
install -Dm644 LICENSE "$stage/usr/share/doc/canvaz/copyright"
mkdir -p apt-assets
dpkg-deb --root-owner-group --build "$stage" "apt-assets/canvaz_${version}_${arch}.deb"
dpkg-deb --info "apt-assets/canvaz_${version}_${arch}.deb"
dpkg-deb --contents "apt-assets/canvaz_${version}_${arch}.deb"
