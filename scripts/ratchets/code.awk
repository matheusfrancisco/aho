# Emits "path<TAB>code" for every Rust source line that is not inside a `#[cfg(test)]` item,
# with `//` comments (including doc comments and their examples) removed.
#
# A `#[cfg(test)]` item ends at `;` (e.g. `mod tests;`) or when the braces it opened close.
# Braces inside string, raw-string (possibly multi-line), and char literals are not counted.

function hashes(n,    s) {
    s = ""
    while (n-- > 0) s = s "#"
    return s
}

# Net brace depth change of `s`; sets OPENED_NOW to the number of `{` seen outside literals.
function braces(s,    i, n, ch, d, o, prev, j) {
    d = 0; o = 0; n = length(s)
    for (i = 1; i <= n; i++) {
        ch = substr(s, i, 1)
        if (in_raw) {
            if (ch == "\"" && substr(s, i + 1, raw_hashes) == hashes(raw_hashes)) { in_raw = 0; i += raw_hashes }
            continue
        }
        if (in_str) {
            if (ch == "\\") { i++; continue }
            if (ch == "\"") in_str = 0
            continue
        }
        if (ch == "/" && substr(s, i + 1, 1) == "/") break
        prev = i > 1 ? substr(s, i - 1, 1) : " "
        if (ch == "r" && prev !~ /[A-Za-z0-9_]/ && match(substr(s, i + 1), /^#*"/)) {
            raw_hashes = RLENGTH - 1; in_raw = 1; i += RLENGTH
            continue
        }
        if (ch == "\"") { in_str = 1; continue }
        if (ch == "'") {
            if (substr(s, i + 1, 1) == "\\") {
                j = index(substr(s, i + 3), "'")
                if (j > 0) i += j + 2
            } else if (substr(s, i + 2, 1) == "'") {
                i += 2
            }
            continue
        }
        if (ch == "{") { d++; o++ }
        else if (ch == "}") d--
    }
    OPENED_NOW = o
    return d
}

# Feeds one line of a skipped item; clears `skip` when the item ends.
function consume(s,    d) {
    d = braces(s)
    if (!opened) {
        if (OPENED_NOW > 0) {
            opened = 1
        } else {
            if (!in_raw && !in_str && s ~ /;[ \t]*$/) skip = 0
            return
        }
    }
    depth += d
    if (depth <= 0 && !in_raw && !in_str) skip = 0
}

FNR == 1 { skip = 0; opened = 0; depth = 0; in_raw = 0; in_str = 0 }

{
    line = $0
    if (skip) {
        consume(line)
        next
    }
    if (match(line, /^[ \t]*#\[cfg\(test\)\]/)) {
        skip = 1; opened = 0; depth = 0; in_raw = 0; in_str = 0
        consume(substr(line, RSTART + RLENGTH))
        next
    }
    sub(/\/\/.*$/, "", line)
    print FILENAME "\t" line
}
