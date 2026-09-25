#!/usr/bin/env bash
#
# vm.sh — manage the disposable Ubuntu dev VM with multipass (macOS host).
#
# Create and provision the disposable Ubuntu VM used for Linux builds. The
# repository is mounted at `/home/ubuntu/xfar`; `provision-vm.sh` installs
# in-guest build and test dependencies.
#
# Usage:
#   ./scripts/vm.sh create      # launch VM, mount repo, provision
#   ./scripts/vm.sh provision   # re-run provisioning in an existing VM
#   ./scripts/vm.sh shell       # open a shell in the VM
#   ./scripts/vm.sh destroy     # delete and purge the VM
#   ./scripts/vm.sh status      # show VM info
#
# Overridable via environment:
#   XFAR_VM_NAME (xfar-dev)  XFAR_VM_CPUS (4)  XFAR_VM_MEM (4G)
#   XFAR_VM_DISK (20G)       XFAR_VM_IMAGE (lts)

set -euo pipefail

VM_NAME="${XFAR_VM_NAME:-xfar-dev}"
VM_CPUS="${XFAR_VM_CPUS:-4}"
VM_MEM="${XFAR_VM_MEM:-4G}"
VM_DISK="${XFAR_VM_DISK:-20G}"
VM_IMAGE="${XFAR_VM_IMAGE:-lts}"
MOUNT_TARGET="/home/ubuntu/xfar"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
err() { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; }

need_multipass() {
    if ! command -v multipass >/dev/null 2>&1; then
        err "multipass not found. Install it with: brew install --cask multipass"
        exit 1
    fi
}

vm_exists() { multipass info "$VM_NAME" >/dev/null 2>&1; }

# Immediately after launch, `multipass exec` can fail with "No route to host"
# until the guest's network/agent settles. Retry until a trivial exec succeeds.
wait_for_ready() {
    local tries="${1:-30}"
    log "Waiting for VM '$VM_NAME' to become reachable"
    for ((i = 1; i <= tries; i++)); do
        if multipass exec "$VM_NAME" -- true >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    err "VM '$VM_NAME' did not become reachable after $((tries * 2))s"
    return 1
}

cmd_create() {
    need_multipass
    if vm_exists; then
        err "VM '$VM_NAME' already exists. Use 'provision', 'shell', or 'destroy'."
        exit 1
    fi
    log "Launching Ubuntu '$VM_IMAGE' VM '$VM_NAME' (${VM_CPUS} CPU, ${VM_MEM} RAM, ${VM_DISK} disk)"
    multipass launch "$VM_IMAGE" \
        --name "$VM_NAME" \
        --cpus "$VM_CPUS" \
        --memory "$VM_MEM" \
        --disk "$VM_DISK"

    log "Mounting repo ($REPO_ROOT) at $VM_NAME:$MOUNT_TARGET"
    multipass mount "$REPO_ROOT" "$VM_NAME:$MOUNT_TARGET"

    cmd_provision
    log "VM '$VM_NAME' is ready. Open a shell with: ./scripts/vm.sh shell"
}

cmd_provision() {
    need_multipass
    if ! vm_exists; then
        err "VM '$VM_NAME' does not exist. Run './scripts/vm.sh create' first."
        exit 1
    fi
    wait_for_ready
    log "Provisioning '$VM_NAME' via scripts/provision-vm.sh"
    # multipass exec runs as the default 'ubuntu' user, which provision-vm.sh
    # requires (it refuses to run as root).
    multipass exec "$VM_NAME" -- bash "$MOUNT_TARGET/scripts/provision-vm.sh"
}

cmd_shell() {
    need_multipass
    multipass shell "$VM_NAME"
}

cmd_destroy() {
    need_multipass
    log "Deleting and purging VM '$VM_NAME'"
    multipass delete "$VM_NAME"
    multipass purge
}

cmd_status() {
    need_multipass
    multipass info "$VM_NAME"
}

usage() {
    cat <<'EOF'
vm.sh — manage the disposable Ubuntu dev VM with multipass (macOS host).

Usage:
  ./scripts/vm.sh create      # launch VM, mount repo, provision
  ./scripts/vm.sh provision   # re-run provisioning in an existing VM
  ./scripts/vm.sh shell       # open a shell in the VM
  ./scripts/vm.sh destroy     # delete and purge the VM
  ./scripts/vm.sh status      # show VM info

Overridable via environment:
  XFAR_VM_NAME (xfar-dev)  XFAR_VM_CPUS (4)  XFAR_VM_MEM (4G)
  XFAR_VM_DISK (20G)       XFAR_VM_IMAGE (lts)
EOF
}

case "${1:-}" in
    create) cmd_create ;;
    provision) cmd_provision ;;
    shell) cmd_shell ;;
    destroy) cmd_destroy ;;
    status) cmd_status ;;
    -h | --help | help | "") usage ;;
    *)
        err "Unknown command: $1"
        usage
        exit 1
        ;;
esac
