#!/usr/bin/env bash
# Capture the native demo only after its DBC, trace expansions and plots are ready.
set -euo pipefail
if [[ ${1:-} != --under-xvfb ]]; then
  if [[ $# != 2 ]]; then
    echo "Usage: bash $0 <demo-build-binary> <output.png>" >&2
    exit 2
  fi
  exec env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
    GDK_BACKEND=x11 GDK_SCALE=1 GDK_DPI_SCALE=1 LIBGL_ALWAYS_SOFTWARE=1 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 TZ=Europe/Stockholm \
    xvfb-run --auto-servernum --server-args="-screen 0 1600x900x24 -dpi 96" \
    bash "$0" --under-xvfb "$@"
fi
shift
binary=$(realpath "$1")
output=$(realpath -m "$2")
work_dir=$(mktemp -d)
app_pid=""
cleanup() {
  local result=$?
  if [[ $result != 0 ]]; then cat "$work_dir/app.log" >&2; fi
  if [[ -n $app_pid ]]; then
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  rm -rf "$work_dir"
  return "$result"
}
trap cleanup EXIT
"$binary" >"$work_dir/app.log" 2>&1 &
app_pid=$!
window_id=""
for ((attempt = 0; attempt < 200; attempt++)); do
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "Demo application exited before capture" >&2
    exit 1
  fi
  window_id=$(xdotool search --all --onlyvisible --pid "$app_pid" \
    --name "^Rusty's Canvaz - CAN Analyzer — Demo ready$" 2>/dev/null | head -n 1 || true)
  if [[ -n $window_id ]]; then break; fi
  sleep 0.2
done
if [[ -z $window_id ]]; then
  echo "Timed out waiting for the demo scene to render" >&2
  exit 1
fi
# Allow WebKit's compositor to present the ready scene; move the cursor off it.
xdotool mousemove 1599 899
sleep 1
mkdir -p "$(dirname "$output")"
import -silent -window "$window_id" "PNG:$work_dir/capture.png"
dimensions=$(identify -format '%wx%h' "$work_dir/capture.png")
colors=$(identify -format '%k' "$work_dir/capture.png")
if [[ $dimensions != 1600x900 || $colors -lt 32 ]]; then
  echo "Invalid screenshot: $dimensions, $colors colors" >&2
  exit 1
fi
mv "$work_dir/capture.png" "$output"
echo "Captured $output ($dimensions)"
