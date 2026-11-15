#!/usr/bin/env bash
# Render the production CLI/TUI from sanitized cache evidence, in a scratch HOME.
# scripts/capture-tui.sh [--snapshot|--synthetic] [output.txt]
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=snapshot
case "${1:-}" in
  --snapshot) shift ;;
  --synthetic) mode=synthetic; shift ;;
  --*) echo 'usage: capture-tui.sh [--snapshot|--synthetic] [output.txt]' >&2; exit 2 ;;
esac
out=${1:-$here/docs/tui-capture.txt}
[ $# -le 1 ] || { echo 'too many arguments' >&2; exit 2; }
command -v python3 >/dev/null || { echo 'python3 is required' >&2; exit 1; }
# Never reuse an older release binary or an AGYVA_BIN override for documentation.
cargo build --manifest-path "$here/Cargo.toml" --locked --bin agyva --example capture-tui
python3 -I "$here/scripts/capture-tui.py" "$mode" "$out"
