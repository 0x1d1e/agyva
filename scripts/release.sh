#!/usr/bin/env bash
# Build the release binary and package it, with checksums, into dist/.
#
#   scripts/release.sh [--target <triple>]
#
# Output (dist/):
#   agyva-<version>-<target>          standalone executable binary
#   agyva-<version>-<target>.tar.gz   binary + README + LICENSE + docs
#   SHA256SUMS                        verify with: (cd dist && sha256sum -c SHA256SUMS)
#
# Install without root (after verifying the checksum):
#   tar -xzf agyva-<version>-<target>.tar.gz
#   install -Dm755 agyva-<version>-<target>/agyva ~/.local/bin/agyva
#
# Reproducibility: the build uses `cargo build --release --locked` (exact
# dependency versions from Cargo.lock) and the archive is normalized (sorted
# names, fixed owner, fixed mtime, no gzip timestamp). The mtime comes from
# SOURCE_DATE_EPOCH, else the last git commit, else a fixed constant. Identical
# compiler, sources and build path give an identical tarball; a different
# toolchain or path may change the binary, so the checksum identifies *this*
# build, it is not a claim of bit-for-bit reproducibility across machines.
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
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 1700000000)}

if [ -n "$explicit_target" ]; then
  cargo build --release --locked --target "$explicit_target"
  bin_path="target/$explicit_target/release/agyva"
else
  cargo build --release --locked
  bin_path="target/release/agyva"
fi

rm -rf dist
stage_root="dist/.stage"
stage="$stage_root/$name"
mkdir -p "$stage/docs"
install -m755 "$bin_path" "$stage/agyva"
install -m644 README.md LICENSE "$stage/"
install -m644 docs/*.md "$stage/docs/"
for extra in docs/tui-capture.txt docs/tui.svg docs/tui-usage.svg; do
  [ -f "$extra" ] && install -m644 "$extra" "$stage/docs/"
done
install -Dm644 docs/capture-data/accounts.json "$stage/docs/capture-data/accounts.json"

(
  cd dist
  tar -C .stage --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
      --format=gnu -cf - "$name" | gzip -n -9 > "$name.tar.gz"
)
rm -rf "$stage_root"
install -m755 "$bin_path" "dist/$name"
(
  cd dist
  sha256sum "$name.tar.gz" "$name" > SHA256SUMS
  sha256sum -c SHA256SUMS
)

echo
echo "binary:  $PWD/dist/$name"
echo "tarball: $PWD/dist/$name.tar.gz"
echo "sums:    $PWD/dist/SHA256SUMS"
