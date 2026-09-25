#!/bin/sh
# Ratchets, not thresholds (doc 12). Per-file counts may not grow, files absent from the baseline
# may not introduce any, and totals may not grow. `#[cfg(test)]` items, `tests/` dirs, `tests.rs`
# files, and comments are excluded from the pattern counts.
#
#   panic      .unwrap( .expect( panic!( todo!(
#   swallowed  `let _ = `  `.ok();`
#   long       files over 1200 lines (value = line count)
#   wildcard   `pub use …::*;` — baseline pinned at 0
#
# Usage: scripts/ratchets/run.sh [--update]    (--update rewrites the JSON baselines)
set -u
cd "$(dirname "$0")/../.." || exit 1
DIR=scripts/ratchets
LIMIT=1200
UPDATE=0
[ "${1:-}" = "--update" ] && UPDATE=1
WORK=target/gate/ratchets
mkdir -p "$WORK"

find crates apps -name '*.rs' -not -path '*/target/*' | sort >"$WORK/all.txt"
grep -v -e '/tests/' -e '/tests\.rs$' -e '/benches/' "$WORK/all.txt" >"$WORK/code-files.txt"
# shellcheck disable=SC2046
awk -f "$DIR/code.awk" $(cat "$WORK/code-files.txt") >"$WORK/code.tsv"

tab=$(printf '\t')

# current TSV for one ratchet, with a __total__ row (sum of values, or number of files for `long`).
current() {
    case "$1" in
        long)
            # shellcheck disable=SC2046
            wc -l $(cat "$WORK/all.txt") | awk -v lim="$LIMIT" '$2 != "total" && $1 > lim { print $2 "\t" $1; n++ } END { print "__total__\t" n + 0 }'
            ;;
        *)
            awk -v kind="$1" -f "$DIR/count.awk" "$WORK/code.tsv" | awk -F"$tab" '{ print; s += $2 } END { print "__total__\t" s + 0 }'
            ;;
    esac
}

to_json() { # name < tsv
    awk -F"$tab" -v name="$1" '
        $1 == "__total__" { total = $2; next }
        { files[n++] = "    \"" $1 "\": " $2 }
        END {
            print "{"
            print "  \"ratchet\": \"" name "\","
            print "  \"total\": " total + 0 ","
            if (n == 0) { print "  \"files\": {}" }
            else {
                print "  \"files\": {"
                for (i = 0; i < n; i++) print files[i] (i < n - 1 ? "," : "")
                print "  }"
            }
            print "}"
        }'
}

from_json() { # file > tsv
    sed -n "s/^    \"\\([^\"]*\\)\": \\([0-9][0-9]*\\),*\$/\\1$tab\\2/p" "$1"
    printf '__total__\t%s\n' "$(sed -n 's/^  "total": \([0-9][0-9]*\),*$/\1/p' "$1")"
}

total_of() { awk -F"$tab" '$1 == "__total__" { print $2 }' "$1"; }

failed=""
for name in panic swallowed long wildcard; do
    current "$name" | sort >"$WORK/$name.tsv"
    total=$(total_of "$WORK/$name.tsv")
    baseline="$DIR/$name.json"
    if [ "$UPDATE" = 1 ]; then
        if [ "$name" = wildcard ] && [ "$total" != 0 ]; then
            echo "  $name: $total wildcard re-export(s); the baseline stays 0 — name what you re-export"
            grep -v '^__total__' "$WORK/$name.tsv" | sed 's/^/    /'
            failed="$failed $name"
            continue
        fi
        to_json "$name" <"$WORK/$name.tsv" >"$baseline"
        echo "  $name: baseline written (total $total)"
        continue
    fi
    if [ ! -f "$baseline" ]; then
        echo "  $name: FAIL no baseline at $baseline; run scripts/gate.sh --fix once and commit it"
        failed="$failed $name"
        continue
    fi
    from_json "$baseline" >"$WORK/$name.base.tsv"
    if awk -f "$DIR/compare.awk" "$WORK/$name.base.tsv" "$WORK/$name.tsv" >"$WORK/$name.report"; then
        echo "  $name: ok (total $total, baseline $(total_of "$WORK/$name.base.tsv"))"
    else
        echo "  $name: FAIL"
        failed="$failed $name"
    fi
    cat "$WORK/$name.report"
done

if [ -n "$failed" ]; then
    echo "ratchets failed:$failed"
    exit 1
fi
