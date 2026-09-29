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
      echo "A macOS native-upgrade command exceeded its ${limit_seconds}-second limit." >&2
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
  echo "usage: $0 --candidate-dmg <path> --invalid-trust-dmg <path> --previous-dmg <path> --success-manifest <path> --invalid-trust-manifest <path> --failed-health-manifest <path>" >&2
}

fail() {
  echo "$1" >&2
  exit 1
}

wait_for_condition() {
  local limit_seconds="$1"
  local failure_message="$2"
  shift 2
  local deadline=$((SECONDS + limit_seconds))
  until "$@"; do
    if (( SECONDS >= deadline )); then
      fail "$failure_message"
    fi
    sleep 0.05
  done
}

attempt_equals() {
  local path="$1"
  local field="$2"
  local expected="$3"
  [[ -f "$path" ]] && [[ "$(jq -r --arg field "$field" '.[$field] // empty' "$path" 2>/dev/null)" == "$expected" ]]
}

installed_version_equals() {
  local expected="$1"
  [[ -x "$installed_binary" ]] && [[ "$(run_with_timeout 30 "$installed_binary" --version 2>/dev/null)" == "$expected" ]]
}

stop_sylvops() {
  local state_root="$1"
  if [[ -x "$installed_binary" ]]; then
    run_with_timeout 20 "$installed_binary" --state-dir "$state_root" daemon stop >/dev/null 2>&1 || true
  fi
  while IFS= read -r process_id; do
    [[ -n "$process_id" ]] && kill "$process_id" 2>/dev/null || true
  done < <(pgrep -f "^${installed_binary} --state-dir ${state_root} desktop$" 2>/dev/null || true)
}

initialize_state_root() {
  local state_root="$1"
  mkdir -p "$state_root"
  run_with_timeout 30 "$installed_binary" --state-dir "$state_root" daemon start
  run_with_timeout 30 "$installed_binary" --state-dir "$state_root" daemon stop
}

restore_previous_package() {
  local state_root="$1"
  stop_sylvops "$state_root"
  rm -rf "$installed_app"
  run_with_timeout 120 ditto "$rollback_app" "$installed_app"
  installed_version_equals "$previous_version" || fail "The previous macOS package was not restored."
}

new_upgrade_handoff() {
  local state_root="$1"
  local package_path="$2"
  local manifest_path="$3"
  local relaunch_desktop="$4"
  local preserve_attempt="${5:-false}"
  local data_directory="$state_root/data"
  local staging_root="$data_directory/upgrades"
  mkdir -p "$staging_root"
  if [[ "$preserve_attempt" != true ]]; then
    rm -rf "$staging_root/rollback"
    rm -f "$staging_root/native-upgrade-attempt.json"
  fi
  cp "$package_path" "$staging_root/payload.staged"
  cp "$manifest_path" "$staging_root/release.json"
  cp "$installed_binary" "$staging_root/sylvops-upgrade-helper"
  chmod 755 "$staging_root/sylvops-upgrade-helper"
  run_with_timeout 30 codesign --verify --strict "$staging_root/sylvops-upgrade-helper"

  jq \
    --arg staging_root "$staging_root" \
    --arg installed_executable "$installed_binary" \
    --arg data_directory "$data_directory" \
    --arg config_directory "$state_root/config" \
    --arg runtime_directory "$state_root/run" \
    --argjson relaunch_desktop "$relaunch_desktop" \
    '{
      release: .release,
      staging_root: $staging_root,
      installed_executable: $installed_executable,
      data_directory: $data_directory,
      config_directory: $config_directory,
      runtime_directory: $runtime_directory,
      client_process_ids: [2147483647],
      relaunch_desktop: $relaunch_desktop
    }' "$manifest_path" > "$staging_root/handoff.json"

  handoff_helper="$staging_root/sylvops-upgrade-helper"
  handoff_path="$staging_root/handoff.json"
  attempt_path="$staging_root/native-upgrade-attempt.json"
  target_version="$(jq -r '.release.target_version' "$manifest_path")"
}

candidate_dmg=""
invalid_trust_dmg=""
previous_dmg=""
success_manifest=""
invalid_trust_manifest=""
failed_health_manifest=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate-dmg) candidate_dmg="${2:-}"; shift 2 ;;
    --invalid-trust-dmg) invalid_trust_dmg="${2:-}"; shift 2 ;;
    --previous-dmg) previous_dmg="${2:-}"; shift 2 ;;
    --success-manifest) success_manifest="${2:-}"; shift 2 ;;
    --invalid-trust-manifest) invalid_trust_manifest="${2:-}"; shift 2 ;;
    --failed-health-manifest) failed_health_manifest="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || fail "The native macOS upgrade test must run on macOS."
for required_path in "$candidate_dmg" "$invalid_trust_dmg" "$previous_dmg" "$success_manifest" "$invalid_trust_manifest" "$failed_health_manifest"; do
  [[ -f "$required_path" && -s "$required_path" ]] || fail "A native macOS upgrade input is missing or empty: $required_path"
done

candidate_dmg="$(cd "$(dirname "$candidate_dmg")" && pwd -P)/$(basename "$candidate_dmg")"
invalid_trust_dmg="$(cd "$(dirname "$invalid_trust_dmg")" && pwd -P)/$(basename "$invalid_trust_dmg")"
previous_dmg="$(cd "$(dirname "$previous_dmg")" && pwd -P)/$(basename "$previous_dmg")"
success_manifest="$(cd "$(dirname "$success_manifest")" && pwd -P)/$(basename "$success_manifest")"
invalid_trust_manifest="$(cd "$(dirname "$invalid_trust_manifest")" && pwd -P)/$(basename "$invalid_trust_manifest")"
failed_health_manifest="$(cd "$(dirname "$failed_health_manifest")" && pwd -P)/$(basename "$failed_health_manifest")"

test_root="$(mktemp -d "${TMPDIR:-/tmp}/sylvops-macos-native-upgrade.XXXXXX")"
previous_mount="$test_root/previous-mounted"
install_directory="$test_root/Applications"
installed_app="$install_directory/SylvOps.app"
installed_binary="$installed_app/Contents/MacOS/sylvops"
rollback_app="$test_root/SylvOps.rollback.app"
previous_mounted=false

cleanup() {
  for state_root in "$test_root"/*-state; do
    [[ -d "$state_root" ]] && stop_sylvops "$state_root"
  done
  while IFS= read -r process_id; do
    [[ -n "$process_id" ]] && kill -KILL "$process_id" 2>/dev/null || true
  done < <(pgrep -f "^${test_root}/.*/sylvops-upgrade-helper (update-helper|update-watchdog)" 2>/dev/null || true)
  if $previous_mounted; then
    run_with_timeout 120 hdiutil detach "$previous_mount" -force >/dev/null 2>&1 || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

mkdir -p "$previous_mount" "$install_directory"
run_with_timeout 120 codesign --verify --verbose=2 "$candidate_dmg"
run_with_timeout 120 xcrun stapler validate "$candidate_dmg"
run_with_timeout 120 spctl --assess --type open --context context:primary-signature --verbose=4 "$candidate_dmg"

if run_with_timeout 120 xcrun stapler validate "$invalid_trust_dmg" >/dev/null 2>&1 \
  && run_with_timeout 120 spctl --assess --type open --context context:primary-signature "$invalid_trust_dmg" >/dev/null 2>&1; then
  fail "The invalid-signature/notarization probe unexpectedly retained trusted status."
fi

run_with_timeout 120 hdiutil attach "$previous_dmg" -nobrowse -readonly -mountpoint "$previous_mount" >/dev/null
previous_mounted=true
[[ -d "$previous_mount/SylvOps.app" ]] || fail "The previous DMG has no SylvOps.app."
run_with_timeout 120 ditto "$previous_mount/SylvOps.app" "$installed_app"
run_with_timeout 120 ditto "$installed_app" "$rollback_app"
run_with_timeout 120 hdiutil detach "$previous_mount" >/dev/null
previous_mounted=false

previous_version="$(run_with_timeout 30 "$installed_binary" --version)"
[[ "$previous_version" == sylvops\ * ]] || fail "The previous macOS package version is invalid: $previous_version"
previous_identifier="$(run_with_timeout 30 plutil -extract CFBundleIdentifier raw -o - "$installed_app/Contents/Info.plist")"
[[ "$previous_identifier" == "com.devemit.sylvops" ]] || fail "The previous bundle identity is invalid: $previous_identifier"
previous_codesign="$test_root/previous-codesign.txt"
run_with_timeout 120 codesign --display --verbose=4 "$installed_app" 2> "$previous_codesign"
previous_team="$(sed -n 's/^TeamIdentifier=//p' "$previous_codesign")"
[[ "$previous_team" =~ ^[A-Z0-9]{10}$ ]] || fail "The previous Developer ID TeamIdentifier is invalid."

success_state="$test_root/success-state"
initialize_state_root "$success_state"
new_upgrade_handoff "$success_state" "$candidate_dmg" "$success_manifest" true
run_with_timeout 240 "$handoff_helper" update-helper --handoff "$handoff_path"
installed_version_equals "sylvops $target_version" || fail "The helper did not install the candidate macOS version."
wait_for_condition 30 "The updated desktop did not relaunch through the application bundle." \
  pgrep -f -x "^${installed_binary} --state-dir ${success_state} desktop$"
run_with_timeout 30 "$installed_binary" --state-dir "$success_state" daemon status
run_with_timeout 120 codesign --verify --deep --strict --verbose=2 "$installed_app"
run_with_timeout 120 spctl --assess --type execute --verbose=4 "$installed_app"
candidate_identifier="$(run_with_timeout 30 plutil -extract CFBundleIdentifier raw -o - "$installed_app/Contents/Info.plist")"
[[ "$candidate_identifier" == "$previous_identifier" ]] || fail "The macOS upgrade changed the bundle identifier."
candidate_codesign="$test_root/candidate-codesign.txt"
run_with_timeout 120 codesign --display --verbose=4 "$installed_app" 2> "$candidate_codesign"
candidate_team="$(sed -n 's/^TeamIdentifier=//p' "$candidate_codesign")"
[[ "$candidate_team" == "$previous_team" ]] || fail "The macOS upgrade changed the Developer ID TeamIdentifier."
[[ ! -e "$success_state/data/upgrades/rollback" ]] || fail "N-1 remained after the macOS candidate passed health checks."
restore_previous_package "$success_state"

invalid_state="$test_root/invalid-trust-state"
initialize_state_root "$invalid_state"
new_upgrade_handoff "$invalid_state" "$invalid_trust_dmg" "$invalid_trust_manifest" false
if run_with_timeout 120 "$handoff_helper" update-helper --handoff "$handoff_path"; then
  fail "The helper installed a DMG with an invalid signature/notarization state."
fi
installed_version_equals "$previous_version" || fail "The invalid macOS package changed the installation."
attempt_equals "$attempt_path" phase rolled_back || fail "Invalid macOS trust did not cancel before replacement."
attempt_equals "$attempt_path" diagnostic preparation_failed || fail "Invalid macOS trust did not record a redacted preparation failure."
stop_sylvops "$invalid_state"

interrupted_state="$test_root/interrupted-state"
initialize_state_root "$interrupted_state"
new_upgrade_handoff "$interrupted_state" "$candidate_dmg" "$success_manifest" false
"$handoff_helper" update-helper --handoff "$handoff_path" &
interrupted_helper_pid=$!
wait_for_condition 90 "The macOS helper never began applying the candidate." attempt_equals "$attempt_path" phase applying
replacement_deadline=$((SECONDS + 90))
candidate_replaced=false
while (( SECONDS < replacement_deadline )); do
  kill -0 "$interrupted_helper_pid" 2>/dev/null || fail "The macOS helper exited before the interruption probe."
  kill -STOP "$interrupted_helper_pid"
  if installed_version_equals "sylvops $target_version"; then
    candidate_replaced=true
    break
  fi
  kill -CONT "$interrupted_helper_pid"
  sleep 0.01
done
$candidate_replaced || fail "The macOS helper did not replace the installed bundle before interruption."
kill -KILL "$interrupted_helper_pid" 2>/dev/null || true
wait "$interrupted_helper_pid" 2>/dev/null || true
wait_for_condition 180 "The watchdog did not restore N-1 after macOS helper interruption." installed_version_equals "$previous_version"
wait_for_condition 180 "The watchdog did not record the interrupted macOS rollback." attempt_equals "$attempt_path" phase rolled_back
attempt_equals "$attempt_path" rollback_attempts 1 || fail "The interrupted macOS replacement did not roll back exactly once."
attempt_equals "$attempt_path" diagnostic helper_interrupted || fail "The interrupted macOS replacement did not retain its redacted diagnostic."
stop_sylvops "$interrupted_state"

failed_state="$test_root/failed-health-state"
initialize_state_root "$failed_state"
new_upgrade_handoff "$failed_state" "$candidate_dmg" "$failed_health_manifest" false
if run_with_timeout 240 "$handoff_helper" update-helper --handoff "$handoff_path"; then
  fail "The macOS helper accepted a candidate with mismatched health metadata."
fi
installed_version_equals "$previous_version" || fail "The failed macOS health check did not restore N-1."
attempt_equals "$attempt_path" phase rolled_back || fail "The failed macOS health check did not finish rollback."
attempt_equals "$attempt_path" rollback_attempts 1 || fail "The failed macOS health check did not roll back exactly once."
attempt_equals "$attempt_path" diagnostic health_check_failed || fail "The failed macOS health check did not retain its redacted diagnostic."

new_upgrade_handoff "$failed_state" "$candidate_dmg" "$failed_health_manifest" false true
if run_with_timeout 30 "$handoff_helper" update-helper --handoff "$handoff_path"; then
  fail "The same failed macOS target was allowed to enter an update loop."
fi
attempt_equals "$attempt_path" phase rolled_back || fail "Retry refusal changed the completed macOS rollback."
attempt_equals "$attempt_path" rollback_attempts 1 || fail "Retry refusal changed the macOS rollback count."
installed_version_equals "$previous_version" || fail "Retry refusal changed the restored macOS installation."
stop_sylvops "$failed_state"

echo "[ok] native macOS N-1 helper success, signature/notarization refusal, interruption recovery, health rollback, application-bundle relaunch, and loop prevention verified"
