# Sourced by fake-tlc.sh and record-tlc.sh. A TLC invocation is identified by its inputs:
# every .tla file in the main module's directory (the provider's scratch dir) plus the
# config file. The key is their POSIX `cksum`, so it is the same on every machine.
tlc_args() {
  cfg=""; main=""; lib=""; next_is_cfg=0
  for a in "$@"; do
    if [ "$next_is_cfg" = 1 ]; then cfg="$a"; next_is_cfg=0; continue; fi
    case "$a" in
      -config) next_is_cfg=1 ;;
      -DTLA-Library=*) lib="${a#-DTLA-Library=}" ;;
    esac
    main="$a"
  done
  dir=$(dirname "$main")
  name=$(basename "$main" .tla)
  key=$(cat "$dir"/*.tla "$cfg" | cksum | awk '{print $1}')
}
