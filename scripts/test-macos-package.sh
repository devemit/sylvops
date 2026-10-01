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
      echo "A macOS package test command exceeded its ${limit_seconds}-second limit." >&2
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

usage() {
  echo "usage: $0 --dmg <path> --architecture <x86_64|aarch64> --expected-version <version> [--previous-dmg <path>] [--require-developer-id] [--require-notarization]" >&2
}

dmg_path=""
architecture=""
expected_version=""
previous_dmg=""
require_developer_id=false
require_notarization=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dmg)
      dmg_path="${2:-}"
      shift 2
      ;;
    --architecture)
      architecture="${2:-}"
      shift 2
      ;;
    --expected-version)
      expected_version="${2:-}"
      shift 2
      ;;
    --previous-dmg)
      previous_dmg="${2:-}"
      shift 2
      ;;
    --require-developer-id)
      require_developer_id=true
      shift
      ;;
    --require-notarization)
      require_notarization=true
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "The SylvOps macOS package smoke test must run on a native macOS host." >&2
  exit 1
fi
if [[ ! -f "$dmg_path" || ! -s "$dmg_path" ]]; then
  echo "The DMG is missing or empty: $dmg_path" >&2
  exit 1
fi
case "$architecture" in
  x86_64) lipo_architecture="x86_64" ;;
  aarch64) lipo_architecture="arm64" ;;
  *)
    echo "Unsupported package architecture: $architecture" >&2
    exit 1
    ;;
esac
if [[ -z "$expected_version" ]]; then
  echo "An expected application version is required." >&2
  exit 1
fi
if [[ -n "$previous_dmg" && ( ! -f "$previous_dmg" || ! -s "$previous_dmg" ) ]]; then
  echo "The previous DMG is missing or empty: $previous_dmg" >&2
  exit 1
fi

test_root="$(mktemp -d "${TMPDIR:-/tmp}/sylvops-macos-package.XXXXXX")"
test_root="$(cd "$test_root" && pwd -P)"
mount_dir="$test_root/mounted"
previous_mount_dir="$test_root/previous-mounted"
install_dir="$test_root/Applications"
installed_app="$install_dir/SylvOps.app"
rollback_app="$test_root/SylvOps.rollback.app"
state_root="$test_root/state"
repository_dir="$test_root/repository"
worktree_dir="$test_root/worktree"
mounted=false
previous_mounted=false

desktop_pattern=""
print_desktop_diagnostics() {
  echo "Desktop process list:" >&2
  run_with_timeout 5 pgrep -l -f sylvops >&2 || true
  echo "Desktop log:" >&2
  sed -n '1,80p' "$state_root/data/desktop.log" >&2 2>/dev/null || true
}

cleanup() {
  if [[ -n "$desktop_pattern" ]]; then
    while IFS= read -r desktop_pid; do
      [[ -n "$desktop_pid" ]] && kill "$desktop_pid" 2>/dev/null || true
    done < <(run_with_timeout 10 pgrep -f -x "$desktop_pattern" 2>/dev/null || true)
  fi
  if $mounted; then
    run_with_timeout 120 hdiutil detach "$mount_dir" -force >/dev/null 2>&1 || true
  fi
  if $previous_mounted; then
    run_with_timeout 120 hdiutil detach "$previous_mount_dir" -force >/dev/null 2>&1 || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

mkdir -p "$mount_dir" "$previous_mount_dir" "$install_dir" "$state_root"
state_sentinel="$state_root/package-preserve.txt"
printf '%s\n' "preserve SylvOps state" > "$state_sentinel"

run_with_timeout 30 git init --quiet "$repository_dir"
run_with_timeout 30 git -C "$repository_dir" config user.name "SylvOps CI"
run_with_timeout 30 git -C "$repository_dir" config user.email "sylvops-ci@example.invalid"
printf '%s\n' "# macOS package smoke test" > "$repository_dir/README.md"
run_with_timeout 30 git -C "$repository_dir" add README.md
run_with_timeout 30 git -C "$repository_dir" commit --quiet -m "test: seed macOS package repository"
run_with_timeout 60 git -C "$repository_dir" worktree add --quiet -b macos-package-smoke-branch "$worktree_dir"
printf '%s\n' "preserve worktree content" > "$worktree_dir/package-preserve.txt"

run_with_timeout 120 codesign --verify --verbose=2 "$dmg_path"
if $require_notarization; then
  run_with_timeout 120 xcrun stapler validate "$dmg_path"
  run_with_timeout 120 spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg_path"
fi

if [[ -n "$previous_dmg" ]]; then
  run_with_timeout 120 codesign --verify --verbose=2 "$previous_dmg"
  run_with_timeout 120 hdiutil attach "$previous_dmg" -nobrowse -readonly -mountpoint "$previous_mount_dir" >/dev/null
  previous_mounted=true
  [[ -d "$previous_mount_dir/SylvOps.app" ]] || { echo "The previous DMG has no SylvOps.app." >&2; exit 1; }
  run_with_timeout 120 ditto "$previous_mount_dir/SylvOps.app" "$installed_app"
  previous_binary="$installed_app/Contents/MacOS/sylvops"
  previous_version="$(run_with_timeout 30 "$previous_binary" --version)"
  [[ "$previous_version" == sylvops\ * && "$previous_version" != "sylvops $expected_version" ]] || { echo "Previous macOS version is invalid: $previous_version" >&2; exit 1; }
  run_with_timeout 120 ditto "$installed_app" "$rollback_app"
  run_with_timeout 120 hdiutil detach "$previous_mount_dir" >/dev/null
  previous_mounted=false
  rm -rf "$installed_app"
fi

run_with_timeout 120 hdiutil attach "$dmg_path" -nobrowse -readonly -mountpoint "$mount_dir" >/dev/null
mounted=true
if [[ ! -d "$mount_dir/SylvOps.app" || ! -L "$mount_dir/Applications" ]]; then
  echo "The DMG must contain SylvOps.app and an Applications shortcut." >&2
  exit 1
fi
run_with_timeout 120 ditto "$mount_dir/SylvOps.app" "$installed_app"

info_plist="$installed_app/Contents/Info.plist"
installed_binary="$installed_app/Contents/MacOS/sylvops"
installed_guide="$installed_app/Contents/Resources/README.md"
if [[ ! -f "$info_plist" || ! -x "$installed_binary" || ! -f "$installed_guide" ]]; then
  echo "The installed application bundle is incomplete." >&2
  exit 1
fi
run_with_timeout 10 grep -Fq '## Install on macOS' "$installed_guide"
run_with_timeout 10 grep -Fq '## First run' "$installed_guide"
run_with_timeout 10 grep -Fq '## Troubleshooting' "$installed_guide"

bundle_identifier="$(run_with_timeout 30 plutil -extract CFBundleIdentifier raw -o - "$info_plist")"
bundle_name="$(run_with_timeout 30 plutil -extract CFBundleName raw -o - "$info_plist")"
bundle_version="$(run_with_timeout 30 plutil -extract CFBundleShortVersionString raw -o - "$info_plist")"
icon_name="$(run_with_timeout 30 plutil -extract CFBundleIconFile raw -o - "$info_plist")"
[[ "$bundle_identifier" == "com.devemit.sylvops" ]] || { echo "Unexpected bundle identifier: $bundle_identifier" >&2; exit 1; }
[[ "$bundle_name" == "SylvOps" ]] || { echo "Unexpected bundle name: $bundle_name" >&2; exit 1; }
[[ "$bundle_version" == "$expected_version" ]] || { echo "Unexpected bundle version: $bundle_version" >&2; exit 1; }
[[ -n "$icon_name" && -f "$installed_app/Contents/Resources/$icon_name" ]] || { echo "The shared application icon is missing from the bundle." >&2; exit 1; }

installed_architectures="$(run_with_timeout 30 lipo -archs "$installed_binary")"
[[ "$installed_architectures" == "$lipo_architecture" ]] || { echo "Unexpected installed architecture: $installed_architectures" >&2; exit 1; }
run_with_timeout 120 codesign --verify --deep --strict --verbose=2 "$installed_app"
codesign_details="$test_root/codesign-details.txt"
run_with_timeout 120 codesign --display --verbose=4 "$installed_app" 2> "$codesign_details"
grep -Eq 'flags=.*runtime' "$codesign_details"
if $require_developer_id; then
  grep -Fq 'Authority=Developer ID Application:' "$codesign_details"
  grep -Eq '^TeamIdentifier=[A-Z0-9]{10}$' "$codesign_details"
fi
if $require_notarization; then
  run_with_timeout 120 spctl --assess --type execute --verbose=4 "$installed_app"
fi

run_with_timeout 30 open -na "$installed_app" --args --state-dir "$state_root"
status_output="$test_root/daemon-status.txt"
deadline=$((SECONDS + 30))
until run_with_timeout 5 "$installed_binary" --state-dir "$state_root" daemon status > "$status_output" 2>&1; do
  if (( SECONDS >= deadline )); then
    echo "The installed app did not start its daemon through LaunchServices within 30 seconds." >&2
    sed -n '1,80p' "$status_output" >&2
    exit 1
  fi
  sleep 1
done

desktop_pattern="$installed_binary --state-dir $state_root desktop"
deadline=$((SECONDS + 30))
until run_with_timeout 5 pgrep -f -x "$desktop_pattern" >/dev/null 2>&1; do
  if (( SECONDS >= deadline )); then
    echo "The installed app did not start its desktop client through LaunchServices within 30 seconds." >&2
    print_desktop_diagnostics
    exit 1
  fi
  sleep 1
done

run_with_timeout 15 "$installed_binary" --state-dir "$state_root" daemon stop
while IFS= read -r desktop_pid; do
  [[ -n "$desktop_pid" ]] && kill "$desktop_pid"
done < <(run_with_timeout 10 pgrep -f -x "$desktop_pattern" 2>/dev/null || true)
desktop_pattern=""

if [[ -n "$previous_dmg" ]]; then
  rm -rf "$installed_app"
  run_with_timeout 120 ditto "$rollback_app" "$installed_app"
  [[ "$(run_with_timeout 30 "$installed_binary" --version)" == "$previous_version" ]]
  rm -rf "$installed_app"
  run_with_timeout 120 ditto "$mount_dir/SylvOps.app" "$installed_app"
  [[ "$(run_with_timeout 30 "$installed_binary" --version)" == "sylvops $expected_version" ]]
fi

rm -rf "$installed_app"
[[ ! -e "$installed_app" ]] || { echo "Removing the application bundle failed." >&2; exit 1; }
[[ -f "$state_sentinel" ]] || { echo "Removing the app deleted SylvOps state." >&2; exit 1; }
[[ -d "$repository_dir/.git" ]] || { echo "Removing the app deleted the repository." >&2; exit 1; }
[[ -f "$worktree_dir/package-preserve.txt" ]] || { echo "Removing the app deleted worktree content." >&2; exit 1; }
run_with_timeout 30 git -C "$repository_dir" show-ref --verify --quiet refs/heads/macos-package-smoke-branch

echo "[ok] signed macOS $architecture app installed, upgraded, rolled back when applicable, and was removed without deleting user content"
