#!/bin/sh
# A stand-in `claude` for tests/e2e.rs. It talks to no API: it appends the recorded (scrubbed)
# transcript records in fixtures/turn1.jsonl to the path the real binary writes
# ($CLAUDE_CONFIG_DIR/projects/<escaped cwd>/<--session-id>.jsonl), a line at a time, then answers
# each line typed into its pane with fixtures/turn2.jsonl.
#
# Marker files in the working directory change its behavior:
#   .fake-dialog    first show a trust question and wait for "1" (like the real first-run dialog)
#   .fake-slow      stop after three records until .fake-continue exists (crash-resume test)
set -eu
fx="$(cd "$(dirname "$0")" && pwd)/fixtures"
session=""
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id) session=$2; shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
[ -n "$session" ] || { echo "fake-claude: no --session-id" >&2; exit 2; }
cwd=$(pwd -P)
dir="${CLAUDE_CONFIG_DIR:?}/projects/$(printf '%s' "$cwd" | sed 's/[^a-zA-Z0-9]/-/g')"
mkdir -p "$dir"
out="$dir/$session.jsonl"

emit() { # emit <fixture> <first line> <last line>
    sed -n "$2,$3p" "$fx/$1" | while IFS= read -r line; do
        printf '%s\n' "$line" | sed -e "s|__SESSION__|$session|g" -e "s|__CWD__|$cwd|g" >>"$out"
        sleep 0.05
    done
}

if [ -f .fake-dialog ]; then
    printf 'Do you trust the files in this folder?\n\n  1. Yes, proceed\n  2. No, exit\n'
    IFS= read -r answer
    [ "$answer" = "1" ] || exit 1
fi
printf 'fake claude: working on the task\n'
if [ -f .fake-slow ]; then
    emit turn1.jsonl 1 3
    while [ ! -f .fake-continue ]; do sleep 0.05; done
    emit turn1.jsonl 4 1000
else
    emit turn1.jsonl 1 1000
fi
printf 'fake claude: idle\n'
while IFS= read -r line; do
    printf 'fake claude: working on a follow-up\n'
    emit turn2.jsonl 1 1000
    printf 'fake claude: idle\n'
done
