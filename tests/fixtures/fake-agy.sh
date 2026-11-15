#!/bin/sh
# A synthetic stand-in for `agy`, used only by tests. Its behavior comes from
# files placed next to this copy of the script (so tests never mutate the
# process environment):
#   usage.out    stdout for `-p /usage`
#   usage.err    stderr for `-p /usage`
#   usage.code   exit status (default 0)
#   usage.sleep  seconds to hang (to exercise the timeout)
#   usage.calls  appended to on every `/usage` run (call counter)
# It never reads or writes any credential.
d=$(dirname "$0")
case "$1" in
  --version) echo "agy 1.5.9"; exit 0 ;;
esac
if [ "$1" = "-p" ] && [ "$2" = "/usage" ]; then
  echo x >> "$d/usage.calls"
  if [ -f "$d/usage.spawn" ]; then
    # A grandchild in the same process group, to check it is killed too.
    sleep 300 & echo $! > "$d/usage.child"
    sleep 300
    exit 0
  fi
  if [ -f "$d/usage.sleep" ]; then exec sleep "$(cat "$d/usage.sleep")"; fi
  if [ -f "$d/usage.out" ]; then cat "$d/usage.out"; fi
  if [ -f "$d/usage.err" ]; then cat "$d/usage.err" >&2; fi
  if [ -f "$d/usage.code" ]; then exit "$(cat "$d/usage.code")"; fi
  exit 0
fi
echo "fake agy: unsupported arguments" >&2
exit 64
