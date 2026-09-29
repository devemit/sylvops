#!/bin/bash

set -euo pipefail

run_with_timeout() {
  local limit_seconds="$1"
  shift
  timeout --signal=TERM --kill-after=5 "$limit_seconds" "$@"
}

fail() {
  echo "$1" >&2
  exit 1
}

usage() {
  echo "usage: $0 --candidate-appimage <path> --candidate-deb <path> --previous-appimage <path> --previous-deb <path> --appimage-success-manifest <path> --appimage-failed-health-manifest <path> --deb-success-manifest <path> --deb-failed-health-manifest <path>" >&2
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

installed_version() {
  local mode="$1"
  case "$mode" in
    appimage) run_with_timeout 60 env APPIMAGE_EXTRACT_AND_RUN=1 "$installed_appimage" --version ;;
    deb) run_with_timeout 30 /usr/bin/sylvops --version ;;
    *) return 2 ;;
  esac
}

installed_version_equals() {
  local mode="$1"
  local expected="$2"
  [[ "$(installed_version "$mode" 2>/dev/null)" == "$expected" ]]
}

run_installed() {
  local mode="$1"
  local state_root="$2"
  shift 2
  case "$mode" in
    appimage) run_with_timeout 60 env APPIMAGE_EXTRACT_AND_RUN=1 "$installed_appimage" --state-dir "$state_root" "$@" ;;
    deb) run_with_timeout 60 /usr/bin/sylvops --state-dir "$state_root" "$@" ;;
    *) return 2 ;;
  esac
}

desktop_running() {
  local mode="$1"
  local state_root="$2"
  case "$mode" in
    appimage) pgrep -f -- "--state-dir ${state_root} desktop" >/dev/null 2>&1 ;;
    deb) pgrep -f -x -- "/usr/bin/sylvops --state-dir ${state_root} desktop" >/dev/null 2>&1 ;;
    *) return 2 ;;
  esac
}

stop_sylvops() {
  local mode="$1"
  local state_root="$2"
  run_installed "$mode" "$state_root" daemon stop >/dev/null 2>&1 || true
  while IFS= read -r process_id; do
    [[ -n "$process_id" ]] && kill "$process_id" 2>/dev/null || true
  done < <(pgrep -f -- "--state-dir ${state_root} desktop" 2>/dev/null || true)
}

initialize_state_root() {
  local mode="$1"
  local state_root="$2"
  mkdir -p "$state_root"
  run_installed "$mode" "$state_root" daemon start
  run_installed "$mode" "$state_root" daemon stop
}

verify_appimage_integration() {
  local extraction_root="$test_root/appimage-extract"
  rm -rf "$extraction_root"
  mkdir -p "$extraction_root"
  (
    cd "$extraction_root"
    run_with_timeout 120 "$installed_appimage" --appimage-extract >/dev/null
  )
  local package_root="$extraction_root/squashfs-root"
  local desktop="$package_root/usr/share/applications/sylvops.desktop"
  local metainfo="$package_root/usr/share/metainfo/com.devemit.sylvops.metainfo.xml"
  local icon
  icon="$(run_with_timeout 15 find "$package_root/usr/share/icons" -type f -path '*/apps/sylvops.png' -print -quit)"
  [[ -x "$package_root/usr/bin/sylvops" ]] || fail "The upgraded AppImage lost executable permissions."
  [[ -f "$desktop" && -f "$metainfo" && -n "$icon" && -s "$icon" ]] || fail "The upgraded AppImage lost desktop integration."
  run_with_timeout 15 desktop-file-validate "$desktop"
  run_with_timeout 10 grep -Fxq 'Name=SylvOps' "$desktop"
  run_with_timeout 10 grep -Fxq 'Exec=sylvops' "$desktop"
  run_with_timeout 10 grep -Fxq 'Icon=sylvops' "$desktop"
  run_with_timeout 10 grep -Fxq 'StartupWMClass=sylvops' "$desktop"
  run_with_timeout 10 grep -Fq '<id>com.devemit.sylvops</id>' "$metainfo"
  run_with_timeout 10 grep -Fq '<launchable type="desktop-id">sylvops.desktop</launchable>' "$metainfo"
}

verify_deb_integration() {
  local expected_version="$1"
  run_with_timeout 30 dpkg --verify sylvops
  [[ "$(run_with_timeout 30 /usr/bin/sylvops --version)" == "sylvops $expected_version" ]] || fail "The installed Debian version is wrong."
  local installed_files="$test_root/deb-installed-files.txt"
  run_with_timeout 30 dpkg-query --listfiles sylvops > "$installed_files"
  run_with_timeout 10 grep -Fxq '/usr/bin/sylvops' "$installed_files"
  run_with_timeout 10 grep -Fxq '/usr/share/applications/sylvops.desktop' "$installed_files"
  run_with_timeout 10 grep -Fxq '/usr/share/metainfo/com.devemit.sylvops.metainfo.xml' "$installed_files"
  local icon
  icon="$(run_with_timeout 10 grep -m 1 -E '^/usr/share/icons/.+/apps/sylvops\.png$' "$installed_files")"
  [[ -x /usr/bin/sylvops && -n "$icon" && -s "$icon" ]] || fail "The Debian upgrade lost executable or icon integration."
  run_with_timeout 15 desktop-file-validate /usr/share/applications/sylvops.desktop
  run_with_timeout 10 grep -Fq '<id>com.devemit.sylvops</id>' /usr/share/metainfo/com.devemit.sylvops.metainfo.xml
}

verify_deb_desktop_launch() {
  run_with_timeout 30 gtk-launch sylvops
  wait_for_condition 30 "The Debian desktop entry did not launch the updated application." \
    pgrep -f -x '/usr/bin/sylvops desktop'
  run_with_timeout 20 /usr/bin/sylvops daemon stop >/dev/null 2>&1 || true
  while IFS= read -r process_id; do
    [[ -n "$process_id" ]] && kill "$process_id" 2>/dev/null || true
  done < <(pgrep -f -x '/usr/bin/sylvops desktop' 2>/dev/null || true)
}

new_upgrade_handoff() {
  local mode="$1"
  local state_root="$2"
  local package_path="$3"
  local manifest_path="$4"
  local relaunch_desktop="$5"
  local preserve_attempt="${6:-false}"
  local expected_installer installed_executable
  case "$mode" in
    appimage)
      expected_installer='linux_app_image'
      installed_executable="$installed_appimage"
      ;;
    deb)
      expected_installer='linux_deb'
      installed_executable='/usr/bin/sylvops'
      ;;
    *) fail "Unknown Linux installation mode: $mode" ;;
  esac
  [[ "$(jq -r '.release.target.installer' "$manifest_path")" == "$expected_installer" ]] || fail "The Linux upgrade manifest targets the wrong package mode."
  local data_directory="$state_root/data"
  local staging_root="$data_directory/upgrades"
  mkdir -p "$staging_root"
  if [[ "$preserve_attempt" != true ]]; then
    rm -rf "$staging_root/rollback"
    rm -f "$staging_root/native-upgrade-attempt.json"
  fi
  cp "$package_path" "$staging_root/payload.staged"
  cp "$manifest_path" "$staging_root/release.json"
  cp "$installed_executable" "$staging_root/sylvops-upgrade-helper"
  chmod 755 "$staging_root/sylvops-upgrade-helper"
  jq \
    --arg staging_root "$staging_root" \
    --arg installed_executable "$installed_executable" \
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

restore_previous_appimage() {
  local state_root="$1"
  stop_sylvops appimage "$state_root"
  cp "$previous_appimage" "$installed_appimage"
  chmod 755 "$installed_appimage"
  installed_version_equals appimage "$previous_appimage_version" || fail "The AppImage test fixture did not restore N-1."
}

install_previous_deb() {
  run_with_timeout 300 sudo --non-interactive env DEBIAN_FRONTEND=noninteractive apt-get install --yes --allow-downgrades "$previous_deb"
  installed_version_equals deb "$previous_deb_version" || fail "The Debian test fixture did not install N-1."
}

exercise_failed_health() {
  local mode="$1"
  local state_root="$2"
  local package_path="$3"
  local manifest_path="$4"
  local previous_version="$5"
  initialize_state_root "$mode" "$state_root"
  new_upgrade_handoff "$mode" "$state_root" "$package_path" "$manifest_path" false
  if run_with_timeout 300 env APPIMAGE_EXTRACT_AND_RUN=1 "$handoff_helper" update-helper --handoff "$handoff_path"; then
    fail "The $mode helper accepted a candidate with mismatched health metadata."
  fi
  installed_version_equals "$mode" "$previous_version" || fail "The failed $mode health check did not restore N-1."
  attempt_equals "$attempt_path" phase rolled_back || fail "The failed $mode health check did not finish rollback."
  attempt_equals "$attempt_path" rollback_attempts 1 || fail "The failed $mode health check did not roll back exactly once."
  attempt_equals "$attempt_path" diagnostic health_check_failed || fail "The failed $mode health check did not retain its redacted diagnostic."
  if [[ "$mode" == appimage ]]; then
    verify_appimage_integration
  else
    verify_deb_integration "${previous_version#sylvops }"
  fi
  new_upgrade_handoff "$mode" "$state_root" "$package_path" "$manifest_path" false true
  if run_with_timeout 30 env APPIMAGE_EXTRACT_AND_RUN=1 "$handoff_helper" update-helper --handoff "$handoff_path"; then
    fail "The same failed $mode target was allowed to enter an update loop."
  fi
  attempt_equals "$attempt_path" phase rolled_back || fail "Retry refusal changed the completed $mode rollback."
  attempt_equals "$attempt_path" rollback_attempts 1 || fail "Retry refusal changed the $mode rollback count."
  installed_version_equals "$mode" "$previous_version" || fail "Retry refusal changed the restored $mode installation."
  stop_sylvops "$mode" "$state_root"
}

candidate_appimage=""
candidate_deb=""
previous_appimage=""
previous_deb=""
appimage_success_manifest=""
appimage_failed_health_manifest=""
deb_success_manifest=""
deb_failed_health_manifest=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate-appimage) candidate_appimage="${2:-}"; shift 2 ;;
    --candidate-deb) candidate_deb="${2:-}"; shift 2 ;;
    --previous-appimage) previous_appimage="${2:-}"; shift 2 ;;
    --previous-deb) previous_deb="${2:-}"; shift 2 ;;
    --appimage-success-manifest) appimage_success_manifest="${2:-}"; shift 2 ;;
    --appimage-failed-health-manifest) appimage_failed_health_manifest="${2:-}"; shift 2 ;;
    --deb-success-manifest) deb_success_manifest="${2:-}"; shift 2 ;;
    --deb-failed-health-manifest) deb_failed_health_manifest="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

[[ "$(uname -s):$(uname -m)" == 'Linux:x86_64' ]] || fail "The native Linux upgrade test requires Linux x86_64."
for required_path in "$candidate_appimage" "$candidate_deb" "$previous_appimage" "$previous_deb" "$appimage_success_manifest" "$appimage_failed_health_manifest" "$deb_success_manifest" "$deb_failed_health_manifest"; do
  [[ -f "$required_path" && -s "$required_path" ]] || fail "A native Linux upgrade input is missing or empty: $required_path"
done

candidate_appimage="$(realpath -- "$candidate_appimage")"
candidate_deb="$(realpath -- "$candidate_deb")"
previous_appimage="$(realpath -- "$previous_appimage")"
previous_deb="$(realpath -- "$previous_deb")"
appimage_success_manifest="$(realpath -- "$appimage_success_manifest")"
appimage_failed_health_manifest="$(realpath -- "$appimage_failed_health_manifest")"
deb_success_manifest="$(realpath -- "$deb_success_manifest")"
deb_failed_health_manifest="$(realpath -- "$deb_failed_health_manifest")"

test_root="$(mktemp -d "${TMPDIR:-/tmp}/sylvops-linux-native-upgrade.XXXXXX")"
installed_appimage="$test_root/SylvOps.AppImage"
polkit_rule='/etc/polkit-1/rules.d/00-sylvops-native-upgrade-ci.rules'
xvfb_pid=""
export HOME="$test_root/home"
export XDG_DATA_HOME="$test_root/xdg-data"
export XDG_CONFIG_HOME="$test_root/xdg-config"
export XDG_RUNTIME_DIR="$test_root/xdg-run"
mkdir -p "$HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup() {
  for state_root in "$test_root"/*-state; do
    [[ -d "$state_root" ]] || continue
    stop_sylvops appimage "$state_root" || true
    if [[ -x /usr/bin/sylvops ]]; then
      stop_sylvops deb "$state_root" || true
    fi
  done
  while IFS= read -r process_id; do
    [[ -n "$process_id" ]] && kill -KILL "$process_id" 2>/dev/null || true
  done < <(pgrep -f "^${test_root}/.*/sylvops-upgrade-helper (update-helper|update-watchdog)" 2>/dev/null || true)
  if [[ -x /usr/bin/sylvops ]]; then
    run_with_timeout 20 /usr/bin/sylvops daemon stop >/dev/null 2>&1 || true
  fi
  if dpkg-query -W -f='${db:Status-Status}' sylvops 2>/dev/null | grep -Fxq installed; then
    run_with_timeout 120 sudo --non-interactive dpkg --remove sylvops >/dev/null 2>&1 || true
  fi
  sudo --non-interactive rm -f "$polkit_rule" >/dev/null 2>&1 || true
  if [[ -n "$xvfb_pid" ]]; then
    kill "$xvfb_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

test_user="$(id -un)"
[[ "$test_user" =~ ^[a-z_][a-z0-9_-]*$ ]] || fail "The Linux test user name is unsafe for the isolated PolicyKit rule."
rule_source="$test_root/00-sylvops-native-upgrade-ci.rules"
printf '%s\n' \
  'polkit.addRule(function(action, subject) {' \
  "  if (subject.user == '$test_user' && action.id == 'org.freedesktop.policykit.exec' &&" \
  "      (action.lookup('program') == '/usr/bin/dpkg' || action.lookup('program') == '/usr/bin/dpkg-repack')) {" \
  '    return polkit.Result.YES;' \
  '  }' \
  '});' > "$rule_source"
sudo --non-interactive install -m 0644 "$rule_source" "$polkit_rule"
sleep 1
run_with_timeout 30 /usr/bin/pkexec /usr/bin/dpkg --version >/dev/null

display_number=":$((100 + RANDOM % 500))"
Xvfb "$display_number" -screen 0 1280x720x24 -nolisten tcp > "$test_root/xvfb.log" 2>&1 &
xvfb_pid=$!
export DISPLAY="$display_number"
wait_for_condition 15 "The virtual Linux display did not become ready." run_with_timeout 3 xdpyinfo -display "$DISPLAY"

cp "$previous_appimage" "$installed_appimage"
chmod 755 "$installed_appimage"
previous_appimage_version="$(installed_version appimage)"
[[ "$previous_appimage_version" == sylvops\ * ]] || fail "The previous AppImage version is invalid."
candidate_version="$(jq -r '.release.target_version' "$appimage_success_manifest")"

appimage_success_state="$test_root/appimage-success-state"
initialize_state_root appimage "$appimage_success_state"
new_upgrade_handoff appimage "$appimage_success_state" "$candidate_appimage" "$appimage_success_manifest" true
run_with_timeout 300 env APPIMAGE_EXTRACT_AND_RUN=1 "$handoff_helper" update-helper --handoff "$handoff_path"
installed_version_equals appimage "sylvops $target_version" || fail "The helper did not install the candidate AppImage."
wait_for_condition 30 "The updated AppImage desktop did not relaunch." desktop_running appimage "$appimage_success_state"
run_installed appimage "$appimage_success_state" daemon status
verify_appimage_integration
[[ ! -e "$appimage_success_state/data/upgrades/rollback" ]] || fail "N-1 remained after the AppImage passed health checks."
restore_previous_appimage "$appimage_success_state"

appimage_interrupted_state="$test_root/appimage-interrupted-state"
initialize_state_root appimage "$appimage_interrupted_state"
new_upgrade_handoff appimage "$appimage_interrupted_state" "$candidate_appimage" "$appimage_success_manifest" false
env APPIMAGE_EXTRACT_AND_RUN=1 "$handoff_helper" update-helper --handoff "$handoff_path" &
interrupted_helper_pid=$!
wait_for_condition 90 "The AppImage helper never began applying the candidate." attempt_equals "$attempt_path" phase applying
replacement_deadline=$((SECONDS + 120))
candidate_replaced=false
while (( SECONDS < replacement_deadline )); do
  kill -0 "$interrupted_helper_pid" 2>/dev/null || fail "The AppImage helper exited before the interruption probe."
  kill -STOP "$interrupted_helper_pid"
  if installed_version_equals appimage "sylvops $candidate_version"; then
    candidate_replaced=true
    break
  fi
  kill -CONT "$interrupted_helper_pid"
  sleep 0.01
done
$candidate_replaced || fail "The AppImage helper did not replace the image before interruption."
kill -KILL "$interrupted_helper_pid" 2>/dev/null || true
wait "$interrupted_helper_pid" 2>/dev/null || true
wait_for_condition 180 "The watchdog did not restore the interrupted AppImage." installed_version_equals appimage "$previous_appimage_version"
wait_for_condition 180 "The watchdog did not record the interrupted AppImage rollback." attempt_equals "$attempt_path" phase rolled_back
attempt_equals "$attempt_path" rollback_attempts 1 || fail "The interrupted AppImage did not roll back exactly once."
attempt_equals "$attempt_path" diagnostic helper_interrupted || fail "The interrupted AppImage lost its redacted diagnostic."
verify_appimage_integration
stop_sylvops appimage "$appimage_interrupted_state"

exercise_failed_health appimage "$test_root/appimage-failed-health-state" "$candidate_appimage" "$appimage_failed_health_manifest" "$previous_appimage_version"

run_with_timeout 300 sudo --non-interactive env DEBIAN_FRONTEND=noninteractive apt-get install --yes "$previous_deb"
previous_deb_version="$(installed_version deb)"
[[ "$previous_deb_version" == sylvops\ * ]] || fail "The previous Debian version is invalid."
deb_candidate_version="$(jq -r '.release.target_version' "$deb_success_manifest")"
verify_deb_integration "${previous_deb_version#sylvops }"

deb_success_state="$test_root/deb-success-state"
initialize_state_root deb "$deb_success_state"
new_upgrade_handoff deb "$deb_success_state" "$candidate_deb" "$deb_success_manifest" true
run_with_timeout 300 "$handoff_helper" update-helper --handoff "$handoff_path"
installed_version_equals deb "sylvops $target_version" || fail "The helper did not install the candidate Debian package."
wait_for_condition 30 "The updated Debian desktop did not relaunch." desktop_running deb "$deb_success_state"
run_installed deb "$deb_success_state" daemon status
verify_deb_integration "$deb_candidate_version"
verify_deb_desktop_launch
[[ ! -e "$deb_success_state/data/upgrades/rollback" ]] || fail "N-1 remained after the Debian package passed health checks."
stop_sylvops deb "$deb_success_state"
install_previous_deb

exercise_failed_health deb "$test_root/deb-failed-health-state" "$candidate_deb" "$deb_failed_health_manifest" "$previous_deb_version"

echo '[ok] native Linux N-1 AppImage and deb helper success, interruption recovery, health rollback, desktop relaunch, package integration, and loop prevention verified'
