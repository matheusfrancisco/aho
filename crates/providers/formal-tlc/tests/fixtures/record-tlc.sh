#!/bin/sh
# Runs the real TLC (`java` on PATH) in place of the provider's launcher and saves its output
# for fake-tlc.sh, with machine-specific paths replaced by placeholders.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/tlc-key.sh"
tlc_args "$@"
out=$(java "$@" 2>&1)
code=$?
{
  printf '#exit %s\n' "$code"
  printf '%s\n' "$out" | sed -e "s|$dir|<work>|g" -e "s|$lib|<spec-dir>|g" \
    -E -e 's#/(private/)?var/folders/[^ )]*/T/tlc-[0-9]+#<tlc-tmp>#g' -e 's#jar:file:[^!]*!#jar:file:<tla2tools.jar>!#g'
} > "$here/tlc/$name-$key.out"
printf '%s\n' "$out"
exit "$code"
