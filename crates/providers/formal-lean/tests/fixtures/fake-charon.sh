#!/bin/sh
# Offline stand-in for `charon cargo --preset=aeneas --dest-file F [--start-from P]...`:
# writes F listing the items it was asked to start from.
dest=""; starts=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dest-file) dest="$2"; shift ;;
    --start-from) starts="$starts $2"; shift ;;
  esac
  shift
done
[ -f Cargo.toml ] || { echo "error: could not find Cargo.toml in $(pwd)"; exit 1; }
printf 'LLBC start-from:%s\n' "$starts" > "$dest"
echo "Finished extracting to $dest"
