#!/bin/sh
# A stand-in `codex` for tests/e2e.rs. It talks to no API: it writes a rollout where the real
# binary does ($CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<thread id>.jsonl) from the
# recorded (scrubbed) records in fixtures/, a line at a time: session_meta, then turn1; each line
# typed into its pane is answered with turn2.
#
# Marker files in the working directory change its behavior:
#   .fake-hang   start a turn and hang until Escape is pressed, then record turn_aborted
set -eu
fx="$(cd "$(dirname "$0")" && pwd)/fixtures"
while [ $# -gt 0 ]; do
    case "$1" in
        -C) cd "$2"; shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
session="01a0f4ce-$(printf '%04x' $$)-7000-8000-$(date +%s)"
cwd=$(pwd -P)
dir="${CODEX_HOME:?}/sessions/$(date +%Y/%m/%d)"
mkdir -p "$dir"
out="$dir/rollout-$(date +%Y-%m-%dT%H-%M-%S)-$session.jsonl"

emit() { # emit <fixture> <first line> <last line>
    sed -n "$2,$3p" "$fx/$1" | while IFS= read -r line; do
        printf '%s\n' "$line" | sed -e "s|__SESSION__|$session|g" -e "s|__CWD__|$cwd|g" >>"$out"
        sleep 0.05
    done
}

emit meta.jsonl 1 1
if [ -f .fake-hang ]; then
    emit aborted.jsonl 1 3
    printf 'Working (esc to interrupt)\n'
    old=$(stty -g)
    stty -icanon -echo min 1
    while :; do
        c=$(dd bs=1 count=1 2>/dev/null | od -An -c | tr -d ' ')
        [ "$c" = '033' ] && break
    done
    stty "$old"
    emit aborted.jsonl 4 4
    printf 'Interrupted\n'
else
    printf 'Working\n'
    emit turn1.jsonl 1 1000
fi
printf 'Idle\n'
while IFS= read -r line; do
    printf 'Working on a follow-up\n'
    emit turn2.jsonl 1 1000
    printf 'Idle\n'
done
