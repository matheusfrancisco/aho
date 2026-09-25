# Counts pattern hits per file over code.awk output. `-v kind=panic|swallowed|wildcard`.
# Prints "path<TAB>count" for every file with at least one hit.

function hits(s, kind,    c) {
    c = 0
    if (kind == "panic") {
        while (match(s, /\.unwrap\(|\.expect\(|panic!\(|todo!\(/)) { c++; s = substr(s, RSTART + RLENGTH) }
    } else if (kind == "swallowed") {
        while (match(s, /let _ = |\.ok\(\);/)) { c++; s = substr(s, RSTART + RLENGTH) }
    } else if (kind == "wildcard") {
        while (match(s, /pub[ \t]+use[ \t][^;]*::\*;/)) { c++; s = substr(s, RSTART + RLENGTH) }
    }
    return c
}

{
    t = index($0, "\t")
    n = hits(substr($0, t + 1), kind)
    if (n > 0) count[substr($0, 1, t - 1)] += n
}
END {
    for (f in count) print f "\t" count[f]
}
