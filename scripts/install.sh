#!/bin/sh
set -eu

version="${SYLVOPS_VERSION:-0.1.0-beta.1}"
install_dir="${SYLVOPS_INSTALL_DIR:-$HOME/.local/bin}"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) asset="sylvops-linux-x86_64.tar.gz" ;;
  Darwin-x86_64) asset="sylvops-macos-x86_64.tar.gz" ;;
  Darwin-arm64) asset="sylvops-macos-aarch64.tar.gz" ;;
  *) echo "Unsupported SylvOps beta platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
base="https://github.com/devemit/sylvops/releases/download/v$version"
curl --fail --location --proto '=https' --tlsv1.2 "$base/$asset" --output "$temporary/$asset"
curl --fail --location --proto '=https' --tlsv1.2 "$base/SHA256SUMS" --output "$temporary/SHA256SUMS"
expected="$(awk -v asset="$asset" '$2 == asset { print $1; exit }' "$temporary/SHA256SUMS")"
test -n "$expected" || { echo "Release checksum does not include $asset" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$temporary/$asset" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')"
fi
test "$actual" = "$expected" || { echo "SylvOps archive checksum mismatch" >&2; exit 1; }
tar -xzf "$temporary/$asset" -C "$temporary"
mkdir -p "$install_dir"
install -m 0755 "$temporary/sylvops" "$install_dir/sylvops"
echo "Installed SylvOps to $install_dir/sylvops"
echo "Add $install_dir to PATH yourself, then run: sylvops open ."
