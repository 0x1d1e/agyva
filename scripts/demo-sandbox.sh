#!/usr/bin/env bash
# Build a throwaway, fully SYNTHETIC agyva sandbox: fake accounts, a fake `agy`
# script and a scratch HOME. Nothing real is read or written: no OS keyring, no
# real $HOME, no ~/.gemini, no real agy.
#
#   scripts/demo-sandbox.sh <empty-or-new-dir>   # prints `export` lines
#
# Env: AGYVA_BIN  path of the agyva binary (default: target/release/agyva,
#                 else target/debug/agyva).
set -euo pipefail

root=${1:?usage: demo-sandbox.sh <dir>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin=${AGYVA_BIN:-}
if [ -z "$bin" ]; then
  for c in "$here/target/release/agyva" "$here/target/debug/agyva"; do
    [ -x "$c" ] && bin=$c && break
  done
fi
[ -n "$bin" ] && [ -x "$bin" ] || { echo "build agyva first (cargo build --release)" >&2; exit 1; }

mkdir -p "$root"
root=$(cd "$root" && pwd)
if [ -n "$(ls -A "$root")" ]; then echo "refusing to use a non-empty directory: $root" >&2; exit 1; fi
mkdir -p "$root"/{home,agyhome,fakeagy}

# The fake agy answers `--version` and `-p /usage` from files next to it.
cp "$here/tests/fixtures/fake-agy.sh" "$root/fakeagy/agy"
chmod 755 "$root/fakeagy/agy"

usage() { # five-hour and weekly remaining fractions
  cat > "$root/fakeagy/usage.out" <<JSON
{"status":"ok","response":"","usage":{},"command":{"data":{"groups":[
{"name":"Gemini Pro","description":"synthetic","buckets":[
 {"id":"g5","name":"Gemini 5h","window":"5h","remaining_fraction":$1,"reset_time":"2099-01-01T05:00:00Z"},
 {"id":"gw","name":"Gemini weekly","window":"weekly","remaining_fraction":$2,"reset_time":"2099-01-07T00:00:00Z"}]},
{"name":"Claude / GPT","description":"synthetic","disabled":true,"buckets":[
 {"id":"c5","name":"Claude 5h","window":"5h","remaining_fraction":0,"reset_time":"2099-01-01T05:00:00Z"}]}
]}}}
JSON
}

# Unsigned, obviously fake credentials (same shape agyva parses).
fake_login() { # sub email tag
  python3 -I - "$root/agyhome/antigravity-oauth-token" "$1" "$2" "$3" <<'PY'
import base64, json, sys
path, sub, email, tag = sys.argv[1:5]
b = lambda s: base64.urlsafe_b64encode(s.encode()).rstrip(b"=").decode()
header = b('{"alg":"none","typ":"JWT"}')
payload = b(json.dumps({"iss": "https://accounts.example.invalid", "sub": sub, "email": email}))
doc = {
    "token": {"access_token": f"access-{tag}", "token_type": "Bearer",
              "refresh_token": f"refresh-{tag}", "expiry": "2030-01-01T00:00:00Z"},
    "auth_method": "consumer",
    "id_token": f"{header}.{payload}.idtok-{tag}",
}
open(path, "w").write(json.dumps(doc))
PY
}

ag() {
  env -i PATH=/usr/bin:/bin HOME="$root/home" NO_COLOR=1 \
    AGYVA_CONFIG_DIR="$root/cfg" AGYVA_STATE_DIR="$root/state" AGYVA_CACHE_DIR="$root/cache" \
    AGYVA_AGY_HOME="$root/agyhome" AGYVA_AGY_BIN="$root/fakeagy/agy" \
    AGYVA_BACKEND=file AGYVA_PASSPHRASE=synthetic-demo-passphrase AGYVA_UPDATE_CHECK=0 \
    "$bin" "$@" 2>>"$root/setup.log"
}

i=0
for spec in "alice:0.82:0.64" "bob:0.35:0.48" "carol:0.07:0.22"; do
  IFS=: read -r name five weekly <<<"$spec"
  fake_login "sub-$name" "$name@example.invalid" "$name"
  ag add --alias "$name" >/dev/null
  usage "$five" "$weekly"
  ag usage "$name" >/dev/null   # fill the cache for this account
  i=$((i + 1))
done
ag use alice >/dev/null
usage 0.82 0.64
ag usage alice >/dev/null

cat <<ENV
export HOME='$root/home'
export NO_COLOR=1
export AGYVA_CONFIG_DIR='$root/cfg'
export AGYVA_STATE_DIR='$root/state'
export AGYVA_CACHE_DIR='$root/cache'
export AGYVA_AGY_HOME='$root/agyhome'
export AGYVA_AGY_BIN='$root/fakeagy/agy'
export AGYVA_BACKEND=file
export AGYVA_UPDATE_CHECK=0
export AGYVA_PASSPHRASE=synthetic-demo-passphrase
ENV
