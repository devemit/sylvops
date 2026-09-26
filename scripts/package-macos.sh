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
      echo "A macOS packaging command exceeded its ${limit_seconds}-second limit." >&2
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
  echo "usage: $0 --target <triple> --architecture <x86_64|aarch64> --signing-identity <identity> [--keychain <path>] [--notary-key <path> --notary-key-id <id> --notary-issuer-id <id>] [--require-developer-id] [--skip-build]" >&2
}

target=""
architecture=""
signing_identity=""
keychain=""
notary_key=""
notary_key_id=""
notary_issuer_id=""
require_developer_id=false
skip_build=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      target="${2:-}"
      shift 2
      ;;
    --architecture)
      architecture="${2:-}"
      shift 2
      ;;
    --signing-identity)
      signing_identity="${2:-}"
      shift 2
      ;;
    --keychain)
      keychain="${2:-}"
      shift 2
      ;;
    --notary-key)
      notary_key="${2:-}"
      shift 2
      ;;
    --notary-key-id)
      notary_key_id="${2:-}"
      shift 2
      ;;
    --notary-issuer-id)
      notary_issuer_id="${2:-}"
      shift 2
      ;;
    --require-developer-id)
      require_developer_id=true
      shift
      ;;
    --skip-build)
      skip_build=true
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "The SylvOps macOS package must be built on a native macOS host." >&2
  exit 1
fi

case "$target:$architecture" in
  x86_64-apple-darwin:x86_64|aarch64-apple-darwin:aarch64) ;;
  *)
    echo "Target '$target' does not match architecture '$architecture'." >&2
    exit 1
    ;;
esac

if [[ "$architecture" == "aarch64" ]]; then
  lipo_architecture="arm64"
else
  lipo_architecture="x86_64"
fi

if [[ -z "$signing_identity" ]]; then
  echo "A code-signing identity is required, using '-' only for isolated CI packages." >&2
  exit 1
fi
if $require_developer_id && [[ "$signing_identity" != "Developer ID Application:"* ]]; then
  echo "Release packages require a Developer ID Application identity." >&2
  exit 1
fi
if [[ -n "$keychain" && ! -f "$keychain" ]]; then
  echo "The signing keychain does not exist: $keychain" >&2
  exit 1
fi

notary_value_count=0
[[ -n "$notary_key" ]] && notary_value_count=$((notary_value_count + 1))
[[ -n "$notary_key_id" ]] && notary_value_count=$((notary_value_count + 1))
[[ -n "$notary_issuer_id" ]] && notary_value_count=$((notary_value_count + 1))
if [[ $notary_value_count -ne 0 && $notary_value_count -ne 3 ]]; then
  echo "Notarization requires the key path, key ID, and issuer ID together." >&2
  exit 1
fi
if $require_developer_id && [[ $notary_value_count -ne 3 ]]; then
  echo "Release packages require notarization credentials." >&2
  exit 1
fi
if [[ -n "$notary_key" && ! -f "$notary_key" ]]; then
  echo "The notarization key does not exist: $notary_key" >&2
  exit 1
fi

repository_root="$(cd "$(dirname "$0")/.." && pwd -P)"
binary_path="$repository_root/target/$target/release/sylvops"
input_dir="$repository_root/target/macos-installer-input"
output_dir="$repository_root/dist/macos-package"
staging_dir="$repository_root/target/macos-dmg-staging-$architecture"
dmg_path="$repository_root/dist/sylvops-macos-$architecture.dmg"
config_path="$repository_root/packaging/macos/Packager.toml"
entitlements_path="$repository_root/packaging/macos/entitlements.plist"

for generated_path in "$input_dir" "$output_dir" "$staging_dir"; do
  case "$generated_path" in
    "$repository_root"/target/*|"$repository_root"/dist/*) ;;
    *)
      echo "Refusing to clean unexpected generated path: $generated_path" >&2
      exit 1
      ;;
  esac
done

if ! $skip_build; then
  run_with_timeout 1800 cargo build --release --locked -p sylvops-cli --target "$target"
fi
if [[ ! -x "$binary_path" ]]; then
  echo "The release executable is missing or not executable: $binary_path" >&2
  exit 1
fi

binary_architectures="$(run_with_timeout 30 lipo -archs "$binary_path")"
if [[ "$binary_architectures" != "$lipo_architecture" ]]; then
  echo "The release executable has architecture '$binary_architectures', expected '$lipo_architecture'." >&2
  exit 1
fi

rm -rf "$input_dir" "$output_dir" "$staging_dir"
rm -f "$dmg_path"
mkdir -p "$input_dir" "$output_dir" "$staging_dir"
install -m 755 "$binary_path" "$input_dir/sylvops"

(
  cd "$repository_root"
  run_with_timeout 600 cargo packager --config "$config_path" --formats app
)

app_candidates=()
while IFS= read -r app_candidate; do
  app_candidates+=("$app_candidate")
done < <(find "$output_dir" -maxdepth 1 -type d -name '*.app' -print)
if [[ ${#app_candidates[@]} -ne 1 ]]; then
  echo "Expected exactly one generated application bundle, found ${#app_candidates[@]}." >&2
  exit 1
fi
app_path="${app_candidates[0]}"

app_sign_args=(--force --options runtime --entitlements "$entitlements_path" --sign "$signing_identity")
dmg_sign_args=(--force --sign "$signing_identity")
if [[ "$signing_identity" != "-" ]]; then
  app_sign_args+=(--timestamp)
  dmg_sign_args+=(--timestamp)
fi
if [[ -n "$keychain" ]]; then
  app_sign_args+=(--keychain "$keychain")
  dmg_sign_args+=(--keychain "$keychain")
fi

run_with_timeout 120 codesign "${app_sign_args[@]}" "$app_path"
run_with_timeout 120 codesign --verify --deep --strict --verbose=2 "$app_path"

run_with_timeout 120 ditto "$app_path" "$staging_dir/SylvOps.app"
ln -s /Applications "$staging_dir/Applications"
run_with_timeout 600 hdiutil create -volname SylvOps -srcfolder "$staging_dir" -ov -format UDZO "$dmg_path"
run_with_timeout 120 codesign "${dmg_sign_args[@]}" "$dmg_path"
run_with_timeout 120 codesign --verify --verbose=2 "$dmg_path"

if [[ $notary_value_count -eq 3 ]]; then
  notary_output="$repository_root/target/macos-notary-$architecture.json"
  rm -f "$notary_output"
  run_with_timeout 1900 xcrun notarytool submit "$dmg_path" \
    --key "$notary_key" \
    --key-id "$notary_key_id" \
    --issuer "$notary_issuer_id" \
    --wait \
    --timeout 30m \
    --output-format json > "$notary_output"
  notary_status="$(run_with_timeout 30 plutil -extract status raw -o - "$notary_output")"
  if [[ "$notary_status" != "Accepted" ]]; then
    echo "Apple notarization finished with status '$notary_status'." >&2
    exit 1
  fi
  run_with_timeout 300 xcrun stapler staple "$dmg_path"
  run_with_timeout 120 xcrun stapler validate "$dmg_path"
fi

echo "$dmg_path"
