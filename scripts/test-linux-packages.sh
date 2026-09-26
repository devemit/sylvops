#!/bin/bash

set -euo pipefail

run_with_timeout() {
  local limit_seconds="$1"
  shift
  timeout --signal=TERM --kill-after=5 "$limit_seconds" "$@"
}

usage() {
  echo "usage: $0 --appimage <path> --deb <path> --expected-version <version> [--previous-appimage <path> --previous-deb <path>]" >&2
}

appimage=""
deb=""
expected_version=""
previous_appimage=""
previous_deb=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --appimage) appimage="${2:-}"; shift 2 ;;
    --deb) deb="${2:-}"; shift 2 ;;
    --expected-version) expected_version="${2:-}"; shift 2 ;;
    --previous-appimage) previous_appimage="${2:-}"; shift 2 ;;
    --previous-deb) previous_deb="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

[[ "$(uname -s):$(uname -m)" == "Linux:x86_64" ]] || { echo "Linux package tests require native x86_64 Linux." >&2; exit 1; }
[[ -x "$appimage" ]] || { echo "AppImage is missing or not executable: $appimage" >&2; exit 1; }
[[ -s "$deb" ]] || { echo "deb package is missing or empty: $deb" >&2; exit 1; }
[[ -n "$expected_version" ]] || { echo "Expected version is required." >&2; exit 1; }
if [[ -n "$previous_appimage" || -n "$previous_deb" ]]; then
  [[ -x "$previous_appimage" ]] || { echo "Previous AppImage is missing or not executable: $previous_appimage" >&2; exit 1; }
  [[ -s "$previous_deb" ]] || { echo "Previous deb package is missing or empty: $previous_deb" >&2; exit 1; }
fi

test_root="$(mktemp -d "${TMPDIR:-/tmp}/sylvops-linux-package.XXXXXX")"
state_root="$test_root/state"
repository="$test_root/repository"
worktree="$test_root/worktree"
cleanup() {
  run_with_timeout 15 sylvops --state-dir "$state_root" daemon stop >/dev/null 2>&1 || true
  if dpkg-query -W -f='${Status}' sylvops 2>/dev/null | grep -Fq 'install ok installed'; then
    sudo dpkg --remove sylvops >/dev/null 2>&1 || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

mkdir -p "$state_root"
printf '%s\n' preserve > "$state_root/package-preserve.txt"
run_with_timeout 30 git init --quiet "$repository"
run_with_timeout 30 git -C "$repository" config user.name "SylvOps CI"
run_with_timeout 30 git -C "$repository" config user.email "sylvops-ci@example.invalid"
printf '%s\n' seed > "$repository/README.md"
run_with_timeout 30 git -C "$repository" add README.md
run_with_timeout 30 git -C "$repository" commit --quiet -m "test: seed Linux package repository"
run_with_timeout 60 git -C "$repository" worktree add --quiet -b linux-package-smoke-branch "$worktree"
printf '%s\n' preserve > "$worktree/package-preserve.txt"

installed_appimage="$test_root/SylvOps.AppImage"
if [[ -n "$previous_appimage" ]]; then
  cp "$previous_appimage" "$installed_appimage"
  chmod +x "$installed_appimage"
  previous_appimage_version="$(APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 60 "$installed_appimage" --version)"
  [[ "$previous_appimage_version" == sylvops\ * && "$previous_appimage_version" != "sylvops $expected_version" ]] || { echo "Previous AppImage version is invalid: $previous_appimage_version" >&2; exit 1; }
  cp "$installed_appimage" "$test_root/SylvOps.AppImage.rollback"
fi
cp "$appimage" "$test_root/SylvOps.AppImage.new"
chmod +x "$test_root/SylvOps.AppImage.new"
mv -f "$test_root/SylvOps.AppImage.new" "$installed_appimage"
appimage_version="$(APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 60 "$installed_appimage" --version)"
[[ "$appimage_version" == "sylvops $expected_version" ]] || { echo "Unexpected AppImage version: $appimage_version" >&2; exit 1; }
if [[ -n "$previous_appimage" ]]; then
  cp "$test_root/SylvOps.AppImage.rollback" "$installed_appimage"
  chmod +x "$installed_appimage"
  [[ "$(APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 60 "$installed_appimage" --version)" == "$previous_appimage_version" ]]
  cp "$appimage" "$test_root/SylvOps.AppImage.new"
  chmod +x "$test_root/SylvOps.AppImage.new"
  mv -f "$test_root/SylvOps.AppImage.new" "$installed_appimage"
  [[ "$(APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 60 "$installed_appimage" --version)" == "sylvops $expected_version" ]]
fi
(
  cd "$test_root"
  run_with_timeout 120 "$appimage" --appimage-extract >/dev/null
)
find "$test_root/squashfs-root" -name 'com.devemit.sylvops.metainfo.xml' -print -quit | grep -q .
find "$test_root/squashfs-root" -name '*.desktop' -print -quit | grep -q .
find "$test_root/squashfs-root" -iname '*sylvops*.png' -print -quit | grep -q .

dpkg-deb --info "$deb" | grep -Fq "Version: $expected_version"
dpkg-deb --contents "$deb" | grep -Fq 'usr/share/applications/'
dpkg-deb --contents "$deb" | grep -Fq 'usr/share/metainfo/com.devemit.sylvops.metainfo.xml'
if [[ -n "$previous_deb" ]]; then
  sudo dpkg --install "$previous_deb"
  previous_deb_version="$(run_with_timeout 30 sylvops --version)"
  [[ "$previous_deb_version" == sylvops\ * && "$previous_deb_version" != "sylvops $expected_version" ]] || { echo "Previous deb version is invalid: $previous_deb_version" >&2; exit 1; }
fi
sudo dpkg --install "$deb"
[[ "$(run_with_timeout 30 sylvops --version)" == "sylvops $expected_version" ]]
run_with_timeout 30 sylvops --state-dir "$state_root" daemon start
run_with_timeout 30 sylvops --state-dir "$state_root" daemon status
run_with_timeout 30 sylvops --state-dir "$state_root" daemon stop
if [[ -n "$previous_deb" ]]; then
  sudo dpkg --install "$previous_deb"
  [[ "$(run_with_timeout 30 sylvops --version)" == "$previous_deb_version" ]]
  sudo dpkg --install "$deb"
  [[ "$(run_with_timeout 30 sylvops --version)" == "sylvops $expected_version" ]]
fi
sudo dpkg --remove sylvops

[[ -f "$state_root/package-preserve.txt" ]]
[[ -d "$repository/.git" ]]
[[ -f "$worktree/package-preserve.txt" ]]
run_with_timeout 30 git -C "$repository" show-ref --verify --quiet refs/heads/linux-package-smoke-branch
echo "[ok] AppImage and deb installed, upgraded, rolled back when applicable, and preserved user content"
