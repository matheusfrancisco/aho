# Ratchet comparison (doc 12). Input: baseline TSV then current TSV, both "path<TAB>value" with a
# "__total__" row. A file may not grow, a file absent from the baseline may not appear, and the
# total may not grow. Exits 1 on any violation; reports how far the ratchet could tighten.

BEGIN { FS = "\t" }
FNR == NR { base[$1] = $2 + 0; next }
{ cur[$1] = $2 + 0 }
END {
    bad = 0; tighter = 0
    for (f in cur) {
        if (f == "__total__") continue
        if (f in base) {
            if (cur[f] > base[f]) { printf "    grew  %s: %d -> %d\n", f, base[f], cur[f]; bad = 1 }
            else if (cur[f] < base[f]) tighter++
        } else {
            printf "    new   %s: %d\n", f, cur[f]; bad = 1
        }
    }
    for (f in base) if (f != "__total__" && !(f in cur)) tighter++
    if (cur["__total__"] > base["__total__"]) { printf "    total grew: %d -> %d\n", base["__total__"], cur["__total__"]; bad = 1 }
    if (!bad && tighter > 0) printf "    %d file(s) improved; `scripts/gate.sh --fix` tightens the baseline\n", tighter
    exit bad
}
