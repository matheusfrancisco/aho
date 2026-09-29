#!/bin/sh
# Configure the vm world from a stock cloud image: cross-compile vm-guest-agent, build a
# cloud-init seed that installs it, import media, write a recipe, and point the project at it.
# Validated end to end on macOS/HVF with Ubuntu 24.04 arm64 (docs/worlds.md).
#
#   scripts/vm-setup.sh --image <cloud-image> [options]
#
#   --image <file>      cloud image with cloud-init/NoCloud support (required)
#                       e.g. https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-arm64.img
#   --name <name>       recipe and media name prefix (default: dev)
#   --firmware <file>   UEFI firmware (default: probed; aarch64 needs one)
#   --workspace <dir>   guest workspace (default: /workspace); the recipe and the agent unit
#                       always match — a mismatch makes the world refuse the agent
#   --debug-login       set root password "aho" on the serial console (debugging only)
#   --smoke             after setup: grant a lease and prove the boot with `aho shell -- uname -a`
#
# The script never downloads media and never installs host packages: missing pieces are
# reported with the exact command, plan-style.
set -eu
# stay in the caller's project: sessions, leases, and the world binding belong to it; only
# the agent build needs the repo
REPO=$(cd "$(dirname "$0")/.." && pwd)

IMAGE=''; NAME=dev; FIRMWARE=''; WORKSPACE=/workspace; DEBUG_LOGIN=0; SMOKE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE=$2; shift 2 ;;
        --name) NAME=$2; shift 2 ;;
        --firmware) FIRMWARE=$2; shift 2 ;;
        --workspace) WORKSPACE=$2; shift 2 ;;
        --debug-login) DEBUG_LOGIN=1; shift ;;
        --smoke) SMOKE=1; shift ;;
        *) echo "unknown argument \`$1\`; see the header of scripts/vm-setup.sh" >&2; exit 2 ;;
    esac
done
[ -n "$IMAGE" ] || { echo "--image <cloud-image> is required; see the header of scripts/vm-setup.sh" >&2; exit 2; }
[ -f "$IMAGE" ] || { echo "no file at $IMAGE" >&2; exit 2; }

fail=0
need() { # binary "fix command"
    if command -v "$1" >/dev/null 2>&1; then return 0; fi
    echo "  missing $1   fix: $2"
    fail=1
}

section() { printf '\n== %s\n' "$1"; }

# guest arch follows the host: HVF/KVM cannot accelerate a foreign one
case "$(uname -m)" in
    arm64|aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) echo "unsupported host arch $(uname -m)" >&2; exit 2 ;;
esac
TARGET=$ARCH-unknown-linux-musl

section "prerequisites"
need "qemu-system-$ARCH" "brew install qemu   (or your distro's qemu package)"
need qemu-img "brew install qemu"
need aho-vm-supervisor "cargo install --locked --path crates/providers/world-vm --bin aho-vm-supervisor"
need cargo-zigbuild "cargo install --locked cargo-zigbuild"
need zig "brew install zig"
rustup target list --installed 2>/dev/null | grep -q "^$TARGET$" || {
    echo "  missing rust target $TARGET   fix: rustup target add $TARGET"; fail=1; }
if command -v "qemu-system-$ARCH" >/dev/null 2>&1; then
    V=$("qemu-system-$ARCH" --version | sed -n 's/.*version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
    MAJ=${V%% *}; MIN=${V##* }
    if [ "${MAJ:-0}" -lt 9 ] || { [ "${MAJ:-0}" -eq 9 ] && [ "${MIN:-0}" -lt 2 ]; }; then
        echo "  QEMU ${MAJ:-?}.${MIN:-?} is too old   fix: upgrade to >= 9.2 (the netdev uses reconnect-ms)"; fail=1
    fi
fi
AHO=${AHO:-}
if [ -z "$AHO" ]; then
    if command -v aho >/dev/null 2>&1; then AHO=aho
    elif [ -x "$REPO/target/debug/aho" ]; then AHO=$REPO/target/debug/aho
    else echo "  missing aho   fix: cargo build -p aho   (or set AHO=<path>)"; fail=1; fi
fi
if [ -z "$FIRMWARE" ] && [ "$ARCH" = aarch64 ]; then
    BREWQ=$(brew --prefix qemu 2>/dev/null || true)
    for f in "${BREWQ:-/nonexistent}/share/qemu/edk2-aarch64-code.fd" \
             /usr/share/qemu/edk2-aarch64-code.fd /usr/share/AAVMF/AAVMF_CODE.fd; do
        [ -f "$f" ] && FIRMWARE=$f && break
    done
    [ -n "$FIRMWARE" ] || { echo "  no UEFI firmware found   fix: pass --firmware <edk2-aarch64-code.fd>"; fail=1; }
fi
[ "$fail" = 0 ] || exit 1
echo "  ok    qemu >= 9.2, supervisor, zigbuild, target, aho, firmware"

section "guest agent ($TARGET)"
(cd "$REPO" && cargo zigbuild --release --target "$TARGET" -p vm-guest-agent)
AGENT=$REPO/target/$TARGET/release/vm-guest-agent

section "cloud-init seed"
WORK=$(mktemp -d) || exit 1
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/seed"
printf 'instance-id: aho-%s\nlocal-hostname: aho-%s\n' "$NAME" "$NAME" >"$WORK/seed/meta-data"
{
    echo "#cloud-config"
    if [ "$DEBUG_LOGIN" = 1 ]; then
        printf 'chpasswd:\n  expire: false\n  list:\n    - root:aho\ndisable_root: false\n'
    fi
    echo "write_files:"
    echo "  - path: /usr/local/bin/vm-guest-agent"
    echo "    permissions: '0755'"
    echo "    encoding: gz+b64"
    echo "    content: |"
    gzip -c "$AGENT" | base64 | sed 's/^/      /'
    echo "  - path: /etc/systemd/system/aho-agent.service"
    echo "    permissions: '0644'"
    echo "    content: |"
    printf '      [Unit]\n      Description=aho guest agent\n      After=multi-user.target\n\n'
    printf '      [Service]\n      ExecStartPre=/bin/mkdir -p %s\n' "$WORKSPACE"
    printf '      ExecStart=/usr/local/bin/vm-guest-agent --workspace %s --device /dev/virtio-ports/org.aho.agent\n' "$WORKSPACE"
    printf '      StandardOutput=journal+console\n      StandardError=journal+console\n'
    printf '      Restart=always\n      RestartSec=2\n\n      [Install]\n      WantedBy=multi-user.target\n'
    echo "runcmd:"
    echo "  - systemctl daemon-reload"
    echo "  - systemctl enable --now aho-agent.service"
} >"$WORK/seed/user-data"
if command -v hdiutil >/dev/null 2>&1; then
    hdiutil makehybrid -quiet -o "$WORK/seed.iso" -iso -joliet -default-volume-name CIDATA "$WORK/seed"
elif command -v xorriso >/dev/null 2>&1; then
    xorriso -as mkisofs -quiet -o "$WORK/seed.iso" -V CIDATA -J -r "$WORK/seed"
elif command -v genisoimage >/dev/null 2>&1; then
    genisoimage -quiet -o "$WORK/seed.iso" -V CIDATA -J -r "$WORK/seed"
else
    echo "no ISO builder; install xorriso (linux) — macOS ships hdiutil" >&2; exit 1
fi
echo "  ok    seed.iso ($WORKSPACE, login: $([ "$DEBUG_LOGIN" = 1 ] && echo 'root/aho on console' || echo none))"

section "catalog"
# kind from the actual content, not the file extension: a qcow2 recorded as raw does not boot
KIND=$(qemu-img info "$IMAGE" | sed -n 's/^file format: //p')
case "$KIND" in raw|qcow2) ;; *) echo "unsupported base image format \`$KIND\`" >&2; exit 1 ;; esac
have() { "$AHO" media list 2>/dev/null | grep -q "^$1 "; }
have "$NAME-base" || "$AHO" media import "$IMAGE" --name "$NAME-base" --kind "$KIND"
have "$NAME-fw" || { [ -n "$FIRMWARE" ] && "$AHO" media import "$FIRMWARE" --name "$NAME-fw"; }
# the seed embeds a freshly built agent: give it a content-suffixed name so re-runs update it
SEEDNAME=$NAME-seed-$(shasum "$WORK/seed.iso" | cut -c1-8)
have "$SEEDNAME" || "$AHO" media import "$WORK/seed.iso" --name "$SEEDNAME"

section "recipe and config"
FWARG=""
[ -n "$FIRMWARE" ] && FWARG="--firmware media:$NAME-fw"
# shellcheck disable=SC2086
printf 'y\n' | "$AHO" add recipe "$NAME" --base "media:$NAME-base" --cdrom "media:$SEEDNAME" $FWARG
"$AHO" set -y vm.recipe "$NAME"
"$AHO" set -y seams.world vm

if [ "$SMOKE" = 1 ]; then
    section "boot smoke"
    # first touch may create the session and fail (no lease yet): that is the expected order
    "$AHO" shell -- true >/dev/null 2>&1 || true
    SESSION=$("$AHO" session list | awk 'NR==1 {print $1}')
    [ -n "$SESSION" ] || { echo "no session was created; run \`$AHO shell -- true\` and retry" >&2; exit 1; }
    "$AHO" lease grant --session "$SESSION" --expires 2h
    echo "  booting (cold boot takes about a minute; vm.boot_timeout_s caps the wait)"
    "$AHO" shell --session "$SESSION" -- uname -a
    echo "  ok    the guest agent answered; \`$AHO sandbox status\` shows the machine"
fi

section "done"
echo "world: vm (recipe $NAME, workspace $WORKSPACE)"
[ "$SMOKE" = 1 ] || echo "next: \`$AHO shell -- true\` once, then \`$AHO lease grant --expires 2h\`, then \`$AHO shell -- uname -a\`"
echo "back to the host world: \`$AHO set -y seams.world host\`"
