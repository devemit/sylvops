#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd -P)"
source "$script_dir/package-macos.sh"
fixture="$(mktemp -d)"
trap 'rm -f "$fixture/calls" "$fixture/output.dmg" "$fixture/log"; rmdir "$fixture"' EXIT

# Keep this fixture fast; native jobs retain the real timeout and disk-image tool.
run_with_timeout() { shift; "$@"; }
sleep() { :; }
hdiutil() {
  local calls
  calls="$(cat "$fixture/calls")"
  calls=$((calls + 1))
  printf '%s\n' "$calls" > "$fixture/calls"
  if [[ "$calls" -gt 1 && -e "$fixture/output.dmg" ]]; then
    echo "A partial image survived before retry." >&2
    return 19
  fi
  touch "$fixture/output.dmg"
  if [[ "$calls" -le "$failures" ]]; then
    echo "$diagnostic" >&2
    return "$failure_status"
  fi
}

check_case() {
  local label="$1" expected_calls="$2" expected_status="$3" actual_status=0
  failures="$4" diagnostic="$5" failure_status="$6"
  printf '0\n' > "$fixture/calls"
  rm -f "$fixture/output.dmg"
  create_dmg "$fixture" "$fixture/output.dmg" > "$fixture/log" 2>&1 || actual_status=$?
  if [[ "$actual_status" -ne "$expected_status" || "$(cat "$fixture/calls")" -ne "$expected_calls" ]]; then
    cat "$fixture/log" >&2
    echo "FAIL: $label (status $actual_status, calls $(cat "$fixture/calls"))" >&2
    return 1
  fi
  echo "PASS: $label"
}

check_case immediate-success 1 0 0 unused 1
check_case transient-resource-busy 3 0 2 'hdiutil: create failed - Resource busy' 1
check_case exhausted-resource-busy 3 1 9 'hdiutil: create failed - Resource busy' 1
check_case unrelated-failure 1 17 9 'hdiutil: create failed - No space left on device' 17
check_case timeout-is-not-retried 1 143 9 'hdiutil: create failed - Resource busy' 143
