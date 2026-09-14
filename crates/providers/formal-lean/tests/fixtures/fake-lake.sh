#!/bin/sh
# Offline stand-in for `lake` in formal-lean's tests. Prints output in the format Lean 4 /
# Lake print it (lake/*.out) for the fixture project's known targets and for the known
# mutants of project/Proofs/Total.lean (`lake env lean <file>`).
here=$(cd "$(dirname "$0")" && pwd)
case "$1" in
  build)
    case "$2" in
      ""|Proofs.Good) cat "$here/lake/build-ok.out"; exit 0 ;;
      Proofs.Unsolved) cat "$here/lake/unsolved.out"; exit 1 ;;
      Proofs.Sorry) cat "$here/lake/sorry.out"; exit 0 ;;
      *) echo "error: unknown target '$2'"; exit 1 ;;
    esac ;;
  env)
    [ "$2" = lean ] || { echo "fake-lake: only 'env lean <file>' is known"; exit 2; }
    f="$3"
    if grep -q 'a - b' "$f"; then sed "s|@FILE@|$f|g" "$here/lake/total-two-false.out"; exit 1; fi
    if grep -q '&& false' "$f"; then sed "s|@FILE@|$f|g" "$here/lake/ok-small-false.out"; exit 1; fi
    exit 0 ;;
  *) echo "fake-lake: unknown command $1"; exit 2 ;;
esac
