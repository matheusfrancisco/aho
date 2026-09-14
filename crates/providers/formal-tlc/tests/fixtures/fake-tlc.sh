#!/bin/sh
# Offline stand-in for `java … tlc2.TLC` in formal-tlc's tests (`formal.tla.java` points here).
# Replays exactly what the real TLC printed, and its exit code, for the same inputs; the
# recordings in tlc/ were made by record-tlc.sh against tla2tools.jar v1.8.0
# (`cargo test -p formal-tlc --test real_tlc -- --ignored`).
here=$(cd "$(dirname "$0")" && pwd)
. "$here/tlc-key.sh"
tlc_args "$@"
f="$here/tlc/$name-$key.out"
if [ ! -f "$f" ]; then
  echo "fake-tlc: no recording for $name with key $key; re-record: cargo test -p formal-tlc --test real_tlc -- --ignored" >&2
  exit 3
fi
code=$(head -n 1 "$f" | sed 's/^#exit //')
tail -n +2 "$f"
exit "$code"
