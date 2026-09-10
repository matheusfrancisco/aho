#!/bin/sh
# A stand-in for qemu-img: `create -f qcow2 [-F fmt -b base] PATH BYTES` writes a small file
# recording what was asked; `info --output=json …` reports a self-contained image.
set -eu
case "$1" in
  create)
    shift; base=""
    while [ $# -gt 2 ]; do
      case "$1" in -b) base="$2"; shift 2 ;; *) shift ;; esac
    done
    printf 'qcow2-stub bytes=%s base=%s\n' "$2" "$base" > "$1" ;;
  info) printf '{"format": "qcow2", "virtual-size": 1048576}\n' ;;
  *) echo "qemu-img-stub: unsupported $1" >&2; exit 1 ;;
esac
