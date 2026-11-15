#!/usr/bin/env bash
# Build the release binary and package it, with checksums, into dist/.
#
#   scripts/release.sh
#
# Output (dist/):
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

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
target=$(rustc -vV | sed -n 's/^host: //p')
name="agyva-$version-$target"
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 1700000000)}

cargo build --release --locked

rm -rf dist
stage="dist/$name"
mkdir -p "$stage/docs"
install -m755 target/release/agyva "$stage/agyva"
install -m644 README.md LICENSE "$stage/"
install -m644 docs/*.md "$stage/docs/"
[ -f docs/tui-capture.txt ] && install -m644 docs/tui-capture.txt "$stage/docs/"

(
  cd dist
  tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
      --format=gnu -cf - "$name" | gzip -n -9 > "$name.tar.gz"
  sha256sum "$name.tar.gz" > SHA256SUMS
  sha256sum -c SHA256SUMS
)
rm -rf "$stage"

echo
echo "tarball: $PWD/dist/$name.tar.gz"
echo "sums:    $PWD/dist/SHA256SUMS"
