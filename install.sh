#!/bin/sh
# Verified installer for agyva releases from GitHub Releases.
# Usage: ./install.sh [--version <vX.Y.Z>] [--prefix <dir>] [--uninstall] [--help]
set -eu

REPO="0x1d1e/agyva"
PREFIX="${HOME}/.local"
VERSION=""
UNINSTALL=0

usage() {
  cat <<'EOF'
Install or uninstall agyva from verified GitHub Releases.

Usage:
  install.sh [OPTIONS]

Options:
  --version <vX.Y.Z>  Install a specific release tag (default: latest stable)
  --prefix <dir>      Install into <dir>/bin/agyva (default: ~/.local)
  --uninstall         Remove <prefix>/bin/agyva
  -h, --help          Show this help
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] && [ -n "$2" ] || die "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      [ -n "$VERSION" ] || die "--version requires a value"
      shift
      ;;
    --prefix)
      [ "$#" -ge 2 ] && [ -n "$2" ] || die "--prefix requires a value"
      PREFIX="$2"
      shift 2
      ;;
    --prefix=*)
      PREFIX="${1#--prefix=}"
      [ -n "$PREFIX" ] || die "--prefix requires a value"
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

DEST_DIR="$PREFIX/bin"
DEST_BIN="$DEST_DIR/agyva"

if [ "$UNINSTALL" -eq 1 ]; then
  if [ -e "$DEST_BIN" ] || [ -L "$DEST_BIN" ]; then
    rm -f "$DEST_BIN"
    printf 'Removed %s\n' "$DEST_BIN"
  else
    printf 'agyva is not installed at %s\n' "$DEST_BIN"
  fi
  exit 0
fi

OS=$(uname -s)
[ "$OS" = "Linux" ] || die "unsupported operating system: $OS (precompiled releases support Linux only)"

ARCH=$(uname -m)
case "$ARCH" in
  x86_64)
    TARGET="x86_64-unknown-linux-gnu"
    ;;
  aarch64|arm64)
    TARGET="aarch64-unknown-linux-gnu"
    ;;
  *)
    die "unsupported architecture: $ARCH (supported: x86_64, aarch64)"
    ;;
esac

if command -v ldd >/dev/null 2>&1; then
  LDD_OUT=$(ldd --version 2>&1 || true)
  case "$LDD_OUT" in
    *musl*)
      die "musl libc is not supported; prebuilt binaries require glibc ($TARGET)"
      ;;
    *GNU*|*GLIBC*|*glibc*)
      ;;
    *)
      if ls /lib/ld-musl-* /lib64/ld-musl-* >/dev/null 2>&1; then
        die "musl libc is not supported; prebuilt binaries require glibc ($TARGET)"
      fi
      ;;
  esac
elif ls /lib/ld-musl-* /lib64/ld-musl-* >/dev/null 2>&1; then
  die "musl libc is not supported; prebuilt binaries require glibc ($TARGET)"
fi

need_cmd curl
need_cmd tar
need_cmd mktemp

if command -v sha256sum >/dev/null 2>&1; then
  SHA_MODE="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA_MODE="shasum"
else
  die "neither sha256sum nor shasum was found to verify downloads"
fi

compute_sha256() {
  if [ "$SHA_MODE" = "sha256sum" ]; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

if [ -z "$VERSION" ]; then
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
  RELEASE_JSON=$(curl -fsSL --proto '=https' --tlsv1.2 \
    -H "Accept: application/vnd.github+json" \
    "$API_URL") || die "failed to query latest release from $API_URL"
  TAG=$(printf '%s' "$RELEASE_JSON" | tr ',' '\n' | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$TAG" ] || die "could not determine latest release tag from GitHub API"
else
  case "$VERSION" in
    v*) TAG="$VERSION" ;;
    *) TAG="v$VERSION" ;;
  esac
fi

VER="${TAG#v}"
case "$VER" in
  ""|*/*|*..*|*[!0-9A-Za-z._+-]*)
    die "invalid release version: $TAG"
    ;;
esac

NAME="agyva-$VER-$TARGET"
TARBALL="$NAME.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/agyva-install.XXXXXX")
STAGED=""
BACKUP=""

cleanup() {
  rc=$?
  trap - EXIT INT TERM HUP
  if [ -n "$BACKUP" ] && { [ -e "$BACKUP" ] || [ -L "$BACKUP" ]; } && [ ! -e "$DEST_BIN" ] && [ ! -L "$DEST_BIN" ]; then
    mv -f "$BACKUP" "$DEST_BIN" 2>/dev/null || true
  fi
  [ -n "$STAGED" ] && rm -f "$STAGED" 2>/dev/null || true
  [ -n "$BACKUP" ] && rm -f "$BACKUP" 2>/dev/null || true
  rm -rf "$TMPDIR" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT INT TERM HUP

printf 'Downloading %s (%s)...\n' "$TARBALL" "$TAG"
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMPDIR/$TARBALL" "$BASE_URL/$TARBALL" \
  || die "failed to download $BASE_URL/$TARBALL"
curl -fsSL --proto '=https' --tlsv1.2 -o "$TMPDIR/SHA256SUMS" "$BASE_URL/SHA256SUMS" \
  || die "failed to download $BASE_URL/SHA256SUMS"

EXPECTED_SHA=$(awk -v file="$TARBALL" '
  NF >= 2 {
    name = $2
    sub(/^\*/, "", name)
    if (name == file) {
      print tolower($1)
      exit
    }
  }
' "$TMPDIR/SHA256SUMS")

[ -n "$EXPECTED_SHA" ] || die "no checksum entry for $TARBALL in SHA256SUMS"
case "$EXPECTED_SHA" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f])
    ;;
  *)
    die "malformed SHA-256 digest in SHA256SUMS"
    ;;
esac

ACTUAL_SHA=$(compute_sha256 "$TMPDIR/$TARBALL" | tr 'A-F' 'a-f')
[ "$ACTUAL_SHA" = "$EXPECTED_SHA" ] || die "SHA-256 verification failed for $TARBALL"

tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
EXTRACTED_BIN="$TMPDIR/$NAME/agyva"
[ -f "$EXTRACTED_BIN" ] && [ ! -L "$EXTRACTED_BIN" ] || die "expected binary missing from archive: $NAME/agyva"

mkdir -p "$DEST_DIR"
STAGED=$(mktemp "$DEST_DIR/.agyva.new.XXXXXX")
install -m 755 "$EXTRACTED_BIN" "$STAGED"

if [ -e "$DEST_BIN" ] || [ -L "$DEST_BIN" ]; then
  BACKUP=$(mktemp "$DEST_DIR/.agyva.bak.XXXXXX")
  cp -Pp "$DEST_BIN" "$BACKUP" || die "failed to back up existing $DEST_BIN"
fi

if ! mv -f "$STAGED" "$DEST_BIN"; then
  if [ -n "$BACKUP" ] && { [ -e "$BACKUP" ] || [ -L "$BACKUP" ]; }; then
    mv -f "$BACKUP" "$DEST_BIN" || true
    BACKUP=""
  fi
  die "failed to install $DEST_BIN"
fi
STAGED=""
if [ -n "$BACKUP" ] && { [ -e "$BACKUP" ] || [ -L "$BACKUP" ]; }; then
  rm -f "$BACKUP"
  BACKUP=""
fi

printf 'Installed agyva %s to %s\n' "$TAG" "$DEST_BIN"
case ":${PATH:-}:" in
  *":$DEST_DIR:"*) ;;
  *)
    printf 'note: %s is not in PATH\n' "$DEST_DIR"
    ;;
esac
