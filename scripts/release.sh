#!/usr/bin/env bash
# Build the release binary and checksums into dist/.
#
#   scripts/release.sh [--target <triple>]
#
# Output (dist/):
#   agyva-<version>-<target>[.exe]    standalone executable binary
#   SHA256SUMS                        verify with: (cd dist && sha256sum -c SHA256SUMS)
#
# Install without root (after verifying the checksum):
#   install -Dm755 agyva-<version>-<target> ~/.local/bin/agyva
#
# Reproducibility: the build uses `cargo build --release --locked` (exact
# dependency versions from Cargo.lock). A different toolchain or build path may
# change the binary, so the checksum identifies *this* build; it is not a claim
# of bit-for-bit reproducibility across machines.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

explicit_target=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        echo "usage: $0 [--target <triple>]" >&2
        exit 2
      fi
      explicit_target="$2"
      shift 2
      ;;
    --target=*)
      explicit_target="${1#--target=}"
      if [ -z "$explicit_target" ]; then
        echo "usage: $0 [--target <triple>]" >&2
        exit 2
      fi
      shift
      ;;
    -h|--help)
      echo "usage: $0 [--target <triple>]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: $0 [--target <triple>]" >&2
      exit 2
      ;;
  esac
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
host_target=$(rustc -vV | sed -n 's/^host: //p')
target=${explicit_target:-$host_target}
name="agyva-$version-$target"

case "$target" in
  *-windows-*) exe_ext=".exe" ;;
  *) exe_ext="" ;;
esac
bin_name="agyva${exe_ext}"

if [ -n "$explicit_target" ]; then
  cargo build --release --locked --target "$explicit_target"
  bin_path="target/$explicit_target/release/$bin_name"
else
  cargo build --release --locked
  bin_path="target/release/$bin_name"
fi

rm -rf dist
mkdir -p dist
install -m755 "$bin_path" "dist/${name}${exe_ext}"

sha256_gen() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

sha256_check() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$@"
  else
    shasum -a 256 -c "$@"
  fi
}

(
  cd dist
  sha256_gen "${name}${exe_ext}" > SHA256SUMS
  sha256_check SHA256SUMS
)

echo
echo "binary:  $PWD/dist/${name}${exe_ext}"
echo "sums:    $PWD/dist/SHA256SUMS"
