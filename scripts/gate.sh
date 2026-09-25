#!/bin/sh
# The CI gate (doc 12): formatting, clippy with warnings denied, ratchets, and the cargo-metadata
# layer guard. One script locally and in CI.
#
#   scripts/gate.sh          check everything; exit 1 if any section fails
#   scripts/gate.sh --fix    format, then rewrite the ratchet and layer baselines to the current state
#
# A failed workspace lint run always fails the gate. Scoped retries are diagnostics,
# never permission to ship providers/consumers that were left ungated.
set -u
cd "$(dirname "$0")/.." || exit 1

FIX=0
case "${1:-}" in
    --fix) FIX=1 ;;
    "") ;;
    *) echo "usage: scripts/gate.sh [--fix]" >&2; exit 2 ;;
esac

CORE="aho-types aho-log aho-config aho-cap aho-policy aho-compose aho-session aho-inspect"
PRODUCT="aho-base aho-headless aho-web aho-lab surface-print surface-rpc aho"
LOGS=target/gate
mkdir -p "$LOGS"
FAILED=""

section() { printf '\n== %s\n' "$1"; }

# scoped <section> <log-prefix> <command with %p for the package>
scoped() {
    name=$1; shift
    bad=""
    for p in $CORE $PRODUCT; do
        cmd=$(printf '%s' "$*" | sed "s/%p/$p/g")
        if sh -c "$cmd" >"$LOGS/$name-$p.log" 2>&1; then
            printf '  ok    %s\n' "$p"
        else
            printf '  FAIL  %s   (%s)\n' "$p" "$LOGS/$name-$p.log"
            bad="$bad $p"
        fi
    done
    echo "  (every other workspace package is not gated in this run)"
    [ -z "$bad" ]
}

section "ambient authority"
python3 scripts/authority/guard.py || FAILED="$FAILED authority-declarations"
python3 scripts/authority/probe.py || FAILED="$FAILED authority-probe"

section "fmt"
if [ "$FIX" = 1 ]; then
    cargo fmt --all || FAILED="$FAILED fmt"
fi
if cargo fmt --all --check >"$LOGS/fmt.log" 2>&1; then
    echo "  ok    workspace is formatted"
else
    echo "  workspace is not formatted ($(grep -c '^Diff in' "$LOGS/fmt.log") hunks, $LOGS/fmt.log); checking core + product packages"
    scoped fmt 'cargo fmt --check -p %p' || FAILED="$FAILED fmt"
fi

section "clippy (-D warnings)"
if cargo clippy --workspace --all-features --lib --bins --quiet -- -D warnings >"$LOGS/clippy.log" 2>&1; then
    echo "  ok    workspace"
else
    echo "  FAIL workspace-wide lint ($LOGS/clippy.log); scoped retries are diagnostic only"
    FAILED="$FAILED clippy"
    scoped clippy 'cargo clippy -p %p --no-deps --quiet -- -D warnings' || true
fi

section "ratchets"
if [ "$FIX" = 1 ]; then
    sh scripts/ratchets/run.sh --update || FAILED="$FAILED ratchets"
else
    sh scripts/ratchets/run.sh || FAILED="$FAILED ratchets"
fi

section "layer guard (cargo metadata)"
if [ "$FIX" = 1 ]; then
    export UPDATE_RATCHETS=1
fi
if cargo test --quiet -p aho --test layers >"$LOGS/layers.log" 2>&1; then
    if [ "$FIX" = 1 ]; then echo "  baseline written: scripts/ratchets/layers.json"; else echo "  ok    seam and aho-types dependencies"; fi
    sed -n 's/^  "\(.*\)": \[$/  known edge from \1 (scripts\/ratchets\/layers.json)/p' scripts/ratchets/layers.json
else
    echo "  FAIL  ($LOGS/layers.log)"
    grep -E "new edges|baseline edges are gone|error" "$LOGS/layers.log" | head -5 | sed 's/^/    /'
    FAILED="$FAILED layers"
fi

echo
if [ -n "$FAILED" ]; then
    echo "gate: FAIL —$FAILED"
    exit 1
fi
echo "gate: PASS"
