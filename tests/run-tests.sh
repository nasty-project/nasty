#!/usr/bin/env bash
#
# NASty Integration Test Runner
#
# Runs test_shares.py inside the Colima Linux VM so that NFS/SMB/iSCSI/NVMe-oF
# client operations work from macOS. The VM connects to the NASty appliance
# over the network, exercising the full stack including networking.
#
# Prerequisites:
#   - Colima installed and running: colima start
#
# Usage:
#   ./tests/run-tests.sh --host <nasty-ip> [--pool <pool>] [--password <pw>]
#   ./tests/run-tests.sh --host 10.10.10.46
#   ./tests/run-tests.sh --host 10.10.10.46 --pool tank --skip-nvmeof
#
# All arguments after the script name are forwarded to test_shares.py.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TEST_SCRIPT="$SCRIPT_DIR/test_shares.py"

# ── Colours ─────────────────────────────────────────────────────
GREEN="\033[92m"
RED="\033[91m"
CYAN="\033[96m"
YELLOW="\033[93m"
RESET="\033[0m"
BOLD="\033[1m"

info()  { echo -e "${CYAN}→${RESET} $1"; }
ok()    { echo -e "  ${GREEN}✓${RESET} $1"; }
fail()  { echo -e "  ${RED}✗${RESET} $1"; }
warn()  { echo -e "  ${YELLOW}!${RESET} $1"; }

# ── Preflight checks ────────────────────────────────────────────

if ! command -v colima &>/dev/null; then
    fail "Colima not found. Install with: brew install colima"
    exit 1
fi

if ! colima status &>/dev/null; then
    fail "Colima is not running. Start with: colima start"
    exit 1
fi

if [[ ! -f "$TEST_SCRIPT" ]]; then
    fail "Test script not found: $TEST_SCRIPT"
    exit 1
fi

# Require --host
if [[ "$*" != *"--host"* ]]; then
    echo -e "${RED}Usage:${RESET} $0 --host <nasty-ip> [options]"
    echo ""
    echo "Options (forwarded to test_shares.py):"
    echo "  --host HOST        NASty appliance IP (required)"
    echo "  --port PORT        WebUI HTTPS port (default 443)"
    echo "  --password PW      Admin password (default 'admin')"
    echo "  --pool POOL        Pool name (auto-detected if omitted)"
    echo "  --skip-nfs         Skip NFS tests"
    echo "  --skip-smb         Skip SMB tests"
    echo "  --skip-iscsi       Skip iSCSI tests"
    echo "  --skip-nvmeof      Skip NVMe-oF tests"
    echo "  --skip-delete      Leave subvolumes/shares behind after tests"
    echo "  --delete-only      Delete test-* leftovers from a prior --skip-delete run"
    exit 1
fi

# ── Provision VM ─────────────────────────────────────────────────

MARKER="/tmp/.nasty-test-provisioned"

info "Checking Colima VM provisioning..."

if ! colima ssh -- test -f "$MARKER" 2>/dev/null; then
    info "Installing test dependencies in Colima VM (one-time setup)..."

    colima ssh -- sudo bash -c '
        set -e
        export DEBIAN_FRONTEND=noninteractive

        apt-get update -qq

        # NFS client
        apt-get install -y -qq nfs-common 2>/dev/null

        # SMB/CIFS client
        apt-get install -y -qq cifs-utils 2>/dev/null

        # iSCSI initiator
        apt-get install -y -qq open-iscsi 2>/dev/null

        # NVMe-oF client
        apt-get install -y -qq nvme-cli 2>/dev/null

        # Extra kernel modules (nvme-tcp lives in linux-modules-extra on Ubuntu)
        KVER=$(dpkg -l | grep -oP "linux-modules-\K[0-9]+\.[0-9]+\.[0-9]+-[0-9]+-generic" | head -1)
        if [ -n "$KVER" ]; then
            apt-get install -y -qq "linux-modules-extra-${KVER}" 2>/dev/null || true
        fi

        # Python + websockets
        apt-get install -y -qq python3 python3-pip python3-venv 2>/dev/null

        # Load kernel modules needed for NVMe-oF client
        modprobe nvme-tcp 2>/dev/null || true
        modprobe nvme-fabrics 2>/dev/null || true

        # Start iscsid if installed
        systemctl start iscsid 2>/dev/null || true
    '

    # Create a venv with websockets inside the VM
    colima ssh -- bash -c '
        python3 -m venv /tmp/nasty-test-venv
        /tmp/nasty-test-venv/bin/pip install -q websockets
    '

    colima ssh -- touch "$MARKER"
    ok "VM provisioned"
else
    ok "VM already provisioned"

    # Modules don't persist across VM restarts — ensure they're loaded
    colima ssh -- sudo bash -c '
        modprobe nvme-tcp 2>/dev/null || true
        modprobe nvme-fabrics 2>/dev/null || true
        systemctl start iscsid 2>/dev/null || true
    '
fi

# ── Copy test script & run ───────────────────────────────────────

info "Copying test script to VM..."
# Use limactl to copy files into the VM (colima uses lima under the hood)
COLIMA_INSTANCE="colima"
cat "$TEST_SCRIPT" | colima ssh -- bash -c 'cat > /tmp/test_shares.py'
ok "Test script copied"

info "Running tests inside Colima VM..."
echo ""

# Run the test with all arguments forwarded
# The VM needs root for mount/iscsi/nvme operations
colima ssh -- sudo /tmp/nasty-test-venv/bin/python3 /tmp/test_shares.py "$@"
EXIT_CODE=$?

exit $EXIT_CODE
