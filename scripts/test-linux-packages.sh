#!/bin/bash

set -euo pipefail

run_with_timeout() {
  local limit_seconds="$1"
  shift
  timeout --signal=TERM --kill-after=5 "$limit_seconds" "$@"
}

usage() {
  echo "usage: $0 --appimage <path> --deb <path> --expected-version <version>" >&2
}

appimage=""
deb=""
expected_version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --appimage) appimage="${2:-}"; shift 2 ;;
    --deb) deb="${2:-}"; shift 2 ;;
    --expected-version) expected_version="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

[[ -n "$expected_version" ]] || { echo "Expected version is required." >&2; exit 1; }
[[ "$(run_with_timeout 5 uname -s):$(run_with_timeout 5 uname -m)" == "Linux:x86_64" ]] || { echo "Linux package tests require native x86_64 Linux." >&2; exit 1; }
appimage="$(run_with_timeout 10 realpath -- "$appimage")"
deb="$(run_with_timeout 10 realpath -- "$deb")"
[[ -f "$appimage" && -x "$appimage" ]] || { echo "Resolved AppImage is missing, not regular, or not executable: $appimage" >&2; exit 1; }
[[ -f "$deb" && -s "$deb" ]] || { echo "Resolved deb package is missing, not regular, or empty: $deb" >&2; exit 1; }

test_root="$(mktemp -d "${TMPDIR:-/tmp}/sylvops-linux-package.XXXXXX")"
state_root="$test_root/state"
repository="$test_root/repository"
worktree="$test_root/worktree"
export HOME="$test_root/home"
export XDG_DATA_HOME="$test_root/xdg-data"
export XDG_CONFIG_HOME="$test_root/xdg-config"
export XDG_RUNTIME_DIR="$test_root/xdg-run"
installed_state_sentinel="$XDG_DATA_HOME/sylvops/package-preserve.txt"
display_number=":$((100 + RANDOM % 500))"
xvfb_pid=""

stop_desktop() {
  local desktop_pattern="$1"
  while IFS= read -r desktop_pid; do
    [[ -n "$desktop_pid" ]] && kill "$desktop_pid" 2>/dev/null || true
  done < <(run_with_timeout 5 pgrep -f -x -- "$desktop_pattern" 2>/dev/null || true)
}

window_has_shell_identity() {
  local first_field properties remainder
  while read -r first_field remainder; do
    [[ "$first_field" =~ ^0x[0-9a-fA-F]+$ ]] || continue
    properties="$(run_with_timeout 3 xprop -id "$first_field" WM_CLASS 2>/dev/null || true)"
    [[ "$properties" == *'"sylvops", "sylvops"'* ]] && return 0
  done < <(run_with_timeout 5 xwininfo -root -tree)
  return 1
}

configure_source() {
  local source="$1"
  case "$source" in
    appimage)
      source_command=(env APPIMAGE_EXTRACT_AND_RUN=1 "$appimage")
      source_state_args=(--state-dir "$state_root")
      source_launch=(env APPIMAGE_EXTRACT_AND_RUN=1 "$appimage" --state-dir "$state_root" up "$repository")
      desktop_pattern=".* --state-dir $state_root desktop"
      ;;
    installed)
      source_command=(sylvops)
      source_state_args=()
      source_launch=(gtk-launch sylvops)
      desktop_pattern="/usr/bin/sylvops desktop"
      ;;
    *) echo "Unknown Linux package source: $source" >&2; return 2 ;;
  esac
}

run_source_command() {
  local limit_seconds="$1"
  shift
  run_with_timeout "$limit_seconds" "${source_command[@]}" "${source_state_args[@]}" "$@"
}

launch_and_verify() {
  local source="$1"
  local label="$2"
  local desktop_pattern status_output
  local -a source_command source_state_args source_launch
  status_output="$test_root/${source}-daemon-status.txt"
  configure_source "$source"

  run_with_timeout 30 "${source_launch[@]}"

  local daemon_deadline=$((SECONDS + 30))
  until run_source_command 5 daemon status > "$status_output" 2>&1; do
    if (( SECONDS >= daemon_deadline )); then
      echo "$label did not start a healthy daemon within 30 seconds." >&2
      sed -n '1,80p' "$status_output" >&2
      exit 1
    fi
    sleep 1
  done

  local desktop_deadline=$((SECONDS + 15))
  until run_with_timeout 5 pgrep -f -x -- "$desktop_pattern" >/dev/null 2>&1; do
    if (( SECONDS >= desktop_deadline )); then
      echo "$label did not launch the desktop client within 15 seconds." >&2
      exit 1
    fi
    sleep 1
  done

  local shell_identity_deadline=$((SECONDS + 15))
  until window_has_shell_identity; do
    if (( SECONDS >= shell_identity_deadline )); then
      echo "$label did not expose the sylvops desktop shell identity within 15 seconds." >&2
      exit 1
    fi
    sleep 1
  done

  run_source_command 15 daemon stop
  stop_desktop "$desktop_pattern"
}

cleanup() {
  if command -v sylvops >/dev/null 2>&1; then
    run_with_timeout 15 sylvops daemon stop >/dev/null 2>&1 || true
  fi
  APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 15 "$appimage" --state-dir "$state_root" daemon stop >/dev/null 2>&1 || true
  stop_desktop ".* --state-dir $state_root desktop"
  stop_desktop "/usr/bin/sylvops desktop"
  if run_with_timeout 15 dpkg-query -W -f='${Status}' sylvops 2>/dev/null | grep -Fq 'install ok installed'; then
    run_with_timeout 120 sudo --non-interactive dpkg --remove sylvops >/dev/null 2>&1 || true
  fi
  if [[ -n "$xvfb_pid" ]]; then
    kill "$xvfb_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

if command -v sylvops >/dev/null 2>&1; then
  echo "The Linux package smoke host must not have SylvOps preinstalled." >&2
  exit 1
fi

mkdir -p "$state_root"
printf '%s\n' preserve > "$state_root/package-preserve.txt"
mkdir -p "$(dirname "$installed_state_sentinel")" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
printf '%s\n' preserve > "$installed_state_sentinel"
run_with_timeout 30 git init --quiet "$repository"
run_with_timeout 30 git -C "$repository" config user.name "SylvOps CI"
run_with_timeout 30 git -C "$repository" config user.email "sylvops-ci@example.invalid"
printf '%s\n' seed > "$repository/README.md"
run_with_timeout 30 git -C "$repository" add README.md
run_with_timeout 30 git -C "$repository" commit --quiet -m "test: seed Linux package repository"
run_with_timeout 60 git -C "$repository" worktree add --quiet -b linux-package-smoke-branch "$worktree"
printf '%s\n' preserve > "$worktree/package-preserve.txt"

Xvfb "$display_number" -screen 0 1280x720x24 -nolisten tcp > "$test_root/xvfb.log" 2>&1 &
xvfb_pid=$!
export DISPLAY="$display_number"
display_deadline=$((SECONDS + 15))
until run_with_timeout 3 xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; do
  if (( SECONDS >= display_deadline )); then
    echo "The virtual Linux display did not become ready." >&2
    sed -n '1,80p' "$test_root/xvfb.log" >&2
    exit 1
  fi
  sleep 1
done

appimage_version="$(APPIMAGE_EXTRACT_AND_RUN=1 run_with_timeout 60 "$appimage" --version)"
[[ "$appimage_version" == "sylvops $expected_version" ]] || { echo "Unexpected AppImage version: $appimage_version" >&2; exit 1; }
(
  cd "$test_root"
  run_with_timeout 120 "$appimage" --appimage-extract >/dev/null
)
appimage_desktop="$test_root/squashfs-root/usr/share/applications/sylvops.desktop"
appimage_metainfo="$test_root/squashfs-root/usr/share/metainfo/com.devemit.sylvops.metainfo.xml"
appimage_icon="$(run_with_timeout 15 find "$test_root/squashfs-root/usr/share/icons" -type f -path '*apps/sylvops.png' -print -quit)"
[[ -f "$appimage_desktop" ]]
[[ -f "$appimage_metainfo" ]]
[[ -n "$appimage_icon" && -f "$appimage_icon" ]]
[[ -x "$test_root/squashfs-root/usr/bin/sylvops" ]]
run_with_timeout 10 grep -Fxq 'Name=SylvOps' "$appimage_desktop"
run_with_timeout 10 grep -Fxq 'Exec=sylvops' "$appimage_desktop"
run_with_timeout 10 grep -Fxq 'Icon=sylvops' "$appimage_desktop"
run_with_timeout 10 grep -Fxq 'StartupWMClass=sylvops' "$appimage_desktop"
run_with_timeout 10 grep -Fq '<id>com.devemit.sylvops</id>' "$appimage_metainfo"
run_with_timeout 10 grep -Fq '<name>SylvOps</name>' "$appimage_metainfo"
run_with_timeout 10 grep -Fq '<launchable type="desktop-id">sylvops.desktop</launchable>' "$appimage_metainfo"
run_with_timeout 15 desktop-file-validate "$appimage_desktop"

launch_and_verify appimage "The AppImage"

[[ ! -e /usr/bin/sylvops ]]
[[ ! -e /usr/share/applications/sylvops.desktop ]]
[[ ! -e /usr/share/metainfo/com.devemit.sylvops.metainfo.xml ]]
user_integration_path="$(run_with_timeout 15 find "$XDG_DATA_HOME" -type f \( -name sylvops.desktop -o -name com.devemit.sylvops.metainfo.xml -o -name sylvops.png \) -print -quit)"
[[ -z "$user_integration_path" ]] || { echo "AppImage created hidden user integration: $user_integration_path" >&2; exit 1; }

deb_info="$(run_with_timeout 30 dpkg-deb --info "$deb")"
[[ "$deb_info" == *"Version: $expected_version"* ]]
[[ "$deb_info" == *'Package: sylvops'* ]]
[[ "$deb_info" == *'Maintainer: devemit'* ]]
deb_contents="$(run_with_timeout 30 dpkg-deb --contents "$deb")"
[[ "$deb_contents" == *'usr/share/applications/sylvops.desktop'* ]]
[[ "$deb_contents" == *'usr/share/metainfo/com.devemit.sylvops.metainfo.xml'* ]]
run_with_timeout 300 sudo --non-interactive env DEBIAN_FRONTEND=noninteractive apt-get install --yes "$deb"
[[ "$(run_with_timeout 30 sylvops --version)" == "sylvops $expected_version" ]]
ldd_output="$(run_with_timeout 30 ldd /usr/bin/sylvops)"
if [[ "$ldd_output" == *'not found'* ]]; then
  echo "The Debian package has unresolved shared-library dependencies." >&2
  exit 1
fi
run_with_timeout 30 dpkg-query -L sylvops > "$test_root/deb-installed-files.txt"
run_with_timeout 10 grep -Fxq '/usr/bin/sylvops' "$test_root/deb-installed-files.txt"
run_with_timeout 10 grep -Fxq '/usr/share/applications/sylvops.desktop' "$test_root/deb-installed-files.txt"
run_with_timeout 10 grep -Fxq '/usr/share/metainfo/com.devemit.sylvops.metainfo.xml' "$test_root/deb-installed-files.txt"
run_with_timeout 10 grep -Eq '^/usr/share/icons/.+/apps/sylvops\.png$' "$test_root/deb-installed-files.txt"
installed_icon="$(run_with_timeout 10 grep -m 1 -E '^/usr/share/icons/.+/apps/sylvops\.png$' "$test_root/deb-installed-files.txt")"
[[ -f "$installed_icon" ]]
run_with_timeout 10 cmp --silent "$appimage_desktop" /usr/share/applications/sylvops.desktop
run_with_timeout 10 cmp --silent "$appimage_metainfo" /usr/share/metainfo/com.devemit.sylvops.metainfo.xml
run_with_timeout 10 cmp --silent "$appimage_icon" "$installed_icon"
run_with_timeout 15 desktop-file-validate /usr/share/applications/sylvops.desktop

launch_and_verify installed "The installed Debian desktop entry"
run_with_timeout 120 sudo --non-interactive dpkg --remove sylvops

while IFS= read -r installed_path; do
  if [[ ( "$installed_path" =~ ^/usr/(bin|share/applications|share/metainfo)/ || "$installed_path" =~ ^/usr/share/icons/.+/apps/sylvops\.png$ ) && -e "$installed_path" ]]; then
    echo "Debian package removal left application integration behind: $installed_path" >&2
    exit 1
  fi
done < "$test_root/deb-installed-files.txt"
[[ -f "$state_root/package-preserve.txt" ]]
[[ -f "$installed_state_sentinel" ]]
[[ -d "$repository/.git" ]]
[[ -f "$worktree/package-preserve.txt" ]]
run_with_timeout 30 git -C "$repository" show-ref --verify --quiet refs/heads/linux-package-smoke-branch
echo "[ok] AppImage and deb metadata, launch, daemon, and data-preserving removal verified"
