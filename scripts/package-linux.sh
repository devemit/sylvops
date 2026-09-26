#!/bin/bash

set -euo pipefail

run_with_timeout() {
  local limit_seconds="$1"
  shift
  "$@" &
  local command_pid=$!
  (
    sleep "$limit_seconds"
    if kill -0 "$command_pid" 2>/dev/null; then
      echo "A Linux packaging command exceeded its ${limit_seconds}-second limit." >&2
      kill -TERM "$command_pid" 2>/dev/null || true
      sleep 5
      kill -KILL "$command_pid" 2>/dev/null || true
    fi
  ) &
  local watchdog_pid=$!
  local status=0
  wait "$command_pid" || status=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  return "$status"
}

target="x86_64-unknown-linux-gnu"
skip_build=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) skip_build=true; shift ;;
    *) echo "usage: $0 [--skip-build]" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s):$(uname -m)" != "Linux:x86_64" ]]; then
  echo "SylvOps Linux packages require a native x86_64 Linux host." >&2
  exit 1
fi

repository_root="$(cd "$(dirname "$0")/.." && pwd -P)"
binary_path="$repository_root/target/$target/release/sylvops"
input_dir="$repository_root/target/linux-installer-input"
output_dir="$repository_root/dist/linux-package"
appimage_path="$repository_root/dist/sylvops-linux-x86_64.AppImage"
deb_path="$repository_root/dist/sylvops-linux-x86_64.deb"
config_path="$repository_root/packaging/linux/Packager.toml"

if ! $skip_build; then
  run_with_timeout 1800 cargo build --release --locked -p sylvops-cli --target "$target"
fi
[[ -x "$binary_path" ]] || { echo "The Linux release executable is missing: $binary_path" >&2; exit 1; }

rm -rf "$input_dir" "$output_dir"
rm -f "$appimage_path" "$deb_path"
mkdir -p "$input_dir" "$output_dir" "$repository_root/dist"
install -m 755 "$binary_path" "$input_dir/sylvops"

(
  cd "$repository_root"
  APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 900 cargo packager --config "$config_path" --formats appimage,deb
)

mapfile -t appimages < <(find "$output_dir" -maxdepth 1 -type f -name '*.AppImage' -print)
mapfile -t debs < <(find "$output_dir" -maxdepth 1 -type f -name '*.deb' -print)
[[ ${#appimages[@]} -eq 1 ]] || { echo "Expected exactly one AppImage." >&2; exit 1; }
[[ ${#debs[@]} -eq 1 ]] || { echo "Expected exactly one deb package." >&2; exit 1; }
install -m 755 "${appimages[0]}" "$appimage_path"
install -m 644 "${debs[0]}" "$deb_path"
echo "$appimage_path"
echo "$deb_path"
