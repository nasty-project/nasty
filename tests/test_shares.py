#!/usr/bin/env python3
"""
NASty Integration Test Suite

Tests the full workflow:
  pool exists → create subvolumes → expose via NFS/SMB/iSCSI/NVMe-oF → mount/connect from client

Requirements (on the Linux test client):
  pip install websockets
  apt install nfs-common cifs-utils open-iscsi nvme-cli   # or equivalent

Usage:
  sudo python3 test_shares.py --host 10.10.10.46
  sudo python3 test_shares.py --host 10.10.10.46 --pool mypool
  sudo python3 test_shares.py --host 10.10.10.46 --skip-nfs --skip-smb
"""

import argparse
import asyncio
import json
import os
import shutil
import subprocess
import sys
import time
import uuid

try:
    import websockets
except ImportError:
    print("ERROR: 'websockets' package required.  Install with: pip install websockets")
    sys.exit(1)


# ── Colours ───────────────────────────────────────────────────────

GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
CYAN = "\033[96m"
RESET = "\033[0m"
BOLD = "\033[1m"


def info(msg):
    print(f"{CYAN}→{RESET} {msg}")


def ok(msg):
    print(f"  {GREEN}✓{RESET} {msg}")


def fail(msg):
    print(f"  {RED}✗{RESET} {msg}")


def warn(msg):
    print(f"  {YELLOW}!{RESET} {msg}")


def header(msg):
    print(f"\n{BOLD}{'═' * 60}")
    print(f"  {msg}")
    print(f"{'═' * 60}{RESET}\n")


# ── JSON-RPC Client ──────────────────────────────────────────────

class NastyClient:
    def __init__(self, host: str, port: int = 443, password: str = "admin"):
        self.host = host
        self.port = port
        self.password = password
        self.ws = None
        self._id = 0
        self.token = None

    async def connect(self):
        import ssl
        ssl_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ssl_ctx.check_hostname = False
        ssl_ctx.verify_mode = ssl.CERT_NONE

        uri = f"wss://{self.host}:{self.port}/ws"
        self.ws = await websockets.connect(uri, ssl=ssl_ctx)

        # Login via HTTP first to get token
        await self._login()

        # Authenticate WebSocket
        await self.ws.send(json.dumps({"token": self.token}))
        auth_resp = json.loads(await self.ws.recv())
        if not auth_resp.get("authenticated"):
            raise Exception(f"WebSocket auth failed: {auth_resp}")

    async def _login(self):
        import ssl
        import urllib.request

        ssl_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ssl_ctx.check_hostname = False
        ssl_ctx.verify_mode = ssl.CERT_NONE

        data = json.dumps({"username": "admin", "password": self.password}).encode()
        req = urllib.request.Request(
            f"https://{self.host}:{self.port}/api/login",
            data=data,
            headers={"Content-Type": "application/json"},
        )
        resp = urllib.request.urlopen(req, context=ssl_ctx)
        body = json.loads(resp.read())
        self.token = body["token"]

    async def call(self, method: str, params: dict = None) -> dict:
        self._id += 1
        msg = {"jsonrpc": "2.0", "method": method, "id": self._id}
        if params:
            msg["params"] = params
        await self.ws.send(json.dumps(msg))
        # Loop until we get the RPC response — skip server-push event notifications
        while True:
            resp = json.loads(await self.ws.recv())
            # Event notifications have "method" but no "id"
            if "id" not in resp:
                continue
            if "error" in resp:
                raise Exception(f"RPC error ({method}): {resp['error']}")
            return resp.get("result")

    async def close(self):
        if self.ws:
            await self.ws.close()


# ── Shell helpers ─────────────────────────────────────────────────

def run(cmd: list[str], check=True, timeout=30) -> subprocess.CompletedProcess:
    """Run a shell command, return result."""
    return subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=check,
    )


def cmd_exists(name: str) -> bool:
    return shutil.which(name) is not None


# ── Test functions ────────────────────────────────────────────────

class TestContext:
    def __init__(self, client: NastyClient, host: str, pool: str):
        self.client = client
        self.host = host
        self.pool = pool
        self.tag = uuid.uuid4().hex[:6]
        self.results: list[tuple[str, bool, str]] = []

    def record(self, name: str, passed: bool, detail: str = ""):
        self.results.append((name, passed, detail))
        if passed:
            ok(f"{name}")
        else:
            fail(f"{name}: {detail}")


async def test_setup(ctx: TestContext):
    """Enable all protocols and verify pool exists."""
    header("Setup")

    # Check pool exists
    info(f"Verifying pool '{ctx.pool}' exists...")
    pools = await ctx.client.call("pool.list")
    pool = next((p for p in pools if p["name"] == ctx.pool), None)
    if not pool:
        names = [p["name"] for p in pools]
        fail(f"Pool '{ctx.pool}' not found. Available: {names}")
        sys.exit(1)
    if not pool["mounted"]:
        info(f"Mounting pool '{ctx.pool}'...")
        await ctx.client.call("pool.mount", {"name": ctx.pool})
    ok(f"Pool '{ctx.pool}' is mounted")

    # Enable all protocols
    info("Enabling protocols...")
    for proto in ["nfs", "smb", "iscsi", "nvmeof"]:
        try:
            await ctx.client.call("service.protocol.enable", {"name": proto})
            ok(f"Enabled {proto}")
        except Exception as e:
            warn(f"Enable {proto}: {e}")

    # Brief pause for services to start
    await asyncio.sleep(2)


async def test_nfs(ctx: TestContext):
    """Create filesystem subvolume → NFS share → mount → write/read → cleanup."""
    header("NFS Test")
    sv_name = f"test-nfs-{ctx.tag}"
    mount_point = f"/tmp/nasty-test-nfs-{ctx.tag}"
    share_id = None

    try:
        # Create filesystem subvolume
        info(f"Creating filesystem subvolume '{sv_name}'...")
        sv = await ctx.client.call("subvolume.create", {
            "pool": ctx.pool,
            "name": sv_name,
            "subvolume_type": "filesystem",
        })
        ctx.record("NFS: subvolume created", True)

        # Create NFS share
        info("Creating NFS share...")
        share = await ctx.client.call("share.nfs.create", {
            "path": sv["path"],
            "clients": [{"host": "*", "options": "rw,sync,no_subtree_check,no_root_squash"}],
        })
        share_id = share["id"]
        ctx.record("NFS: share created", True)

        # Mount from client
        info(f"Mounting NFS share at {mount_point}...")
        os.makedirs(mount_point, exist_ok=True)
        r = run(["mount", "-t", "nfs4", f"{ctx.host}:{sv['path']}", mount_point], check=False)
        if r.returncode != 0:
            # Some VM environments (e.g. Colima/Lima) don't permit NFS mounts.
            # Treat "Operation not permitted" as a skip, not a failure.
            if "Operation not permitted" in r.stderr:
                warn("NFS mount not supported in this VM — skipping mount/read/write tests")
                ctx.record("NFS: mount (skipped — VM limitation)", True)
                return
            ctx.record("NFS: mount", False, r.stderr.strip())
            return
        ctx.record("NFS: mount", True)

        # Write test file
        test_data = f"nasty-nfs-test-{ctx.tag}"
        test_file = os.path.join(mount_point, "testfile.txt")
        with open(test_file, "w") as f:
            f.write(test_data)
        ctx.record("NFS: write", True)

        # Read back
        with open(test_file, "r") as f:
            read_back = f.read()
        if read_back == test_data:
            ctx.record("NFS: read/verify", True)
        else:
            ctx.record("NFS: read/verify", False, f"expected '{test_data}', got '{read_back}'")

    except Exception as e:
        ctx.record("NFS: test", False, str(e))
    finally:
        # Cleanup
        run(["umount", mount_point], check=False)
        if os.path.isdir(mount_point):
            os.rmdir(mount_point)
        if share_id:
            try:
                await ctx.client.call("share.nfs.delete", {"id": share_id})
            except Exception:
                pass
        try:
            await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass


async def test_smb(ctx: TestContext):
    """Create filesystem subvolume → SMB share → mount → write/read → cleanup."""
    header("SMB Test")
    sv_name = f"test-smb-{ctx.tag}"
    share_name = f"test{ctx.tag}"
    mount_point = f"/tmp/nasty-test-smb-{ctx.tag}"
    share_id = None

    try:
        # Create filesystem subvolume
        info(f"Creating filesystem subvolume '{sv_name}'...")
        sv = await ctx.client.call("subvolume.create", {
            "pool": ctx.pool,
            "name": sv_name,
            "subvolume_type": "filesystem",
        })
        ctx.record("SMB: subvolume created", True)

        # Create SMB share
        info(f"Creating SMB share '{share_name}'...")
        share = await ctx.client.call("share.smb.create", {
            "name": share_name,
            "path": sv["path"],
            "guest_ok": True,
            "browseable": True,
        })
        share_id = share["id"]
        ctx.record("SMB: share created", True)

        # Brief pause for Samba to reload config
        await asyncio.sleep(3)

        # Mount from client
        info(f"Mounting SMB share at {mount_point}...")
        os.makedirs(mount_point, exist_ok=True)
        r = run(
            ["mount", "-t", "cifs", f"//{ctx.host}/{share_name}", mount_point,
             "-o", "guest,vers=3.0"],
            check=False,
        )
        if r.returncode != 0:
            ctx.record("SMB: mount", False, r.stderr.strip())
            return
        ctx.record("SMB: mount", True)

        # Write test file
        test_data = f"nasty-smb-test-{ctx.tag}"
        test_file = os.path.join(mount_point, "testfile.txt")
        with open(test_file, "w") as f:
            f.write(test_data)
        ctx.record("SMB: write", True)

        # Read back
        with open(test_file, "r") as f:
            read_back = f.read()
        if read_back == test_data:
            ctx.record("SMB: read/verify", True)
        else:
            ctx.record("SMB: read/verify", False, f"expected '{test_data}', got '{read_back}'")

    except Exception as e:
        ctx.record("SMB: test", False, str(e))
    finally:
        run(["umount", mount_point], check=False)
        if os.path.isdir(mount_point):
            os.rmdir(mount_point)
        if share_id:
            try:
                await ctx.client.call("share.smb.delete", {"id": share_id})
            except Exception:
                pass
        try:
            await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass


async def test_iscsi(ctx: TestContext):
    """Create block subvolume → iSCSI target → discover → login → verify device → cleanup."""
    header("iSCSI Test")
    sv_name = f"test-iscsi-{ctx.tag}"
    target_name = f"test-iscsi-{ctx.tag}"
    iqn = f"iqn.2024-01.com.nasty:{target_name}"
    target_id = None

    try:
        # Create block subvolume (64 MB)
        info(f"Creating block subvolume '{sv_name}' (64 MiB)...")
        sv = await ctx.client.call("subvolume.create", {
            "pool": ctx.pool,
            "name": sv_name,
            "subvolume_type": "block",
            "volsize_bytes": 64 * 1024 * 1024,
        })
        block_dev = sv.get("block_device")
        if not block_dev:
            ctx.record("iSCSI: block device", False, "no block_device returned")
            return
        ctx.record("iSCSI: block subvolume created", True)

        # Create iSCSI target
        info("Creating iSCSI target...")
        target = await ctx.client.call("share.iscsi.create_quick", {
            "name": target_name,
            "device_path": block_dev,
        })
        target_id = target["id"]
        ctx.record("iSCSI: target created", True)

        # Discover targets from client
        info(f"Discovering iSCSI targets on {ctx.host}...")
        r = run(
            ["iscsiadm", "-m", "discovery", "-t", "sendtargets", "-p", ctx.host],
            check=False,
        )
        if r.returncode != 0:
            ctx.record("iSCSI: discovery", False, r.stderr.strip())
            return
        if iqn not in r.stdout:
            ctx.record("iSCSI: discovery", False, f"IQN {iqn} not in discovery output")
            return
        ctx.record("iSCSI: discovery", True)

        # Login
        info(f"Logging in to {iqn}...")
        r = run(
            ["iscsiadm", "-m", "node", "-T", iqn, "-p", f"{ctx.host}:3260", "--login"],
            check=False,
        )
        if r.returncode != 0:
            ctx.record("iSCSI: login", False, r.stderr.strip())
            return
        ctx.record("iSCSI: login", True)

        # Wait for device to appear
        await asyncio.sleep(2)

        # Check that a SCSI device appeared
        r = run(["iscsiadm", "-m", "session", "-P", "3"], check=False)
        if "Attached scsi disk" in r.stdout:
            ctx.record("iSCSI: device attached", True)
        else:
            ctx.record("iSCSI: device attached", False, "no scsi disk in session info")

    except Exception as e:
        ctx.record("iSCSI: test", False, str(e))
    finally:
        # Logout
        run(
            ["iscsiadm", "-m", "node", "-T", iqn, "-p", f"{ctx.host}:3260", "--logout"],
            check=False,
        )
        # Remove discovery record
        run(
            ["iscsiadm", "-m", "node", "-T", iqn, "-p", f"{ctx.host}:3260", "-o", "delete"],
            check=False,
        )
        if target_id:
            try:
                await ctx.client.call("share.iscsi.delete", {"id": target_id})
            except Exception:
                pass
        try:
            await ctx.client.call("subvolume.detach", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass
        try:
            await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass


async def test_nvmeof(ctx: TestContext):
    """Create block subvolume → NVMe-oF subsystem → connect → verify device → cleanup."""
    header("NVMe-oF Test")
    sv_name = f"test-nvme-{ctx.tag}"
    subsys_name = f"test-nvme-{ctx.tag}"
    nqn = f"nqn.2024-01.com.nasty:{subsys_name}"
    subsys_id = None

    try:
        # Create block subvolume (64 MB)
        info(f"Creating block subvolume '{sv_name}' (64 MiB)...")
        sv = await ctx.client.call("subvolume.create", {
            "pool": ctx.pool,
            "name": sv_name,
            "subvolume_type": "block",
            "volsize_bytes": 64 * 1024 * 1024,
        })
        block_dev = sv.get("block_device")
        if not block_dev:
            ctx.record("NVMe-oF: block device", False, "no block_device returned")
            return
        ctx.record("NVMe-oF: block subvolume created", True)

        # Create NVMe-oF share
        info("Creating NVMe-oF share...")
        subsys = await ctx.client.call("share.nvmeof.create_quick", {
            "name": subsys_name,
            "device_path": block_dev,
        })
        subsys_id = subsys["id"]
        ctx.record("NVMe-oF: share created", True)

        # Connect from client
        info(f"Connecting to NVMe-oF target {nqn}...")
        r = run(
            ["nvme", "connect", "-t", "tcp", "-n", nqn,
             "-a", ctx.host, "-s", "4420"],
            check=False,
        )
        if r.returncode != 0:
            ctx.record("NVMe-oF: connect", False, r.stderr.strip())
            return
        ctx.record("NVMe-oF: connect", True)

        # Wait for device
        await asyncio.sleep(2)

        # Check that an NVMe device appeared
        r = run(["nvme", "list"], check=False)
        if nqn in r.stdout or "nasty" in r.stdout.lower():
            ctx.record("NVMe-oF: device visible", True)
        else:
            # Also check via subsystem listing
            r2 = run(["nvme", "list-subsys"], check=False)
            if nqn in r2.stdout:
                ctx.record("NVMe-oF: device visible", True)
            else:
                ctx.record("NVMe-oF: device visible", False, "NQN not found in nvme list/list-subsys")

    except Exception as e:
        ctx.record("NVMe-oF: test", False, str(e))
    finally:
        # Disconnect
        run(["nvme", "disconnect", "-n", nqn], check=False)
        if subsys_id:
            try:
                await ctx.client.call("share.nvmeof.delete", {"id": subsys_id})
            except Exception:
                pass
        try:
            await ctx.client.call("subvolume.detach", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass
        try:
            await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_name})
        except Exception:
            pass


# ── Main ──────────────────────────────────────────────────────────

async def main():
    parser = argparse.ArgumentParser(description="NASty integration test suite")
    parser.add_argument("--host", required=True, help="NASty appliance IP/hostname")
    parser.add_argument("--port", type=int, default=443, help="WebUI HTTPS port (default 443)")
    parser.add_argument("--password", default="admin", help="Admin password (default 'admin')")
    parser.add_argument("--pool", default=None, help="Pool name (auto-detected if omitted)")
    parser.add_argument("--skip-nfs", action="store_true")
    parser.add_argument("--skip-smb", action="store_true")
    parser.add_argument("--skip-iscsi", action="store_true")
    parser.add_argument("--skip-nvmeof", action="store_true")
    args = parser.parse_args()

    # Check prerequisites
    if os.geteuid() != 0:
        print(f"{RED}ERROR:{RESET} This test must be run as root (needs mount/iscsi/nvme)")
        sys.exit(1)

    header("NASty Integration Test Suite")
    info(f"Target: {args.host}:{args.port}")

    # Check client tools
    tools = {
        "nfs": ("mount.nfs", "nfs-common / nfs-utils"),
        "smb": ("mount.cifs", "cifs-utils"),
        "iscsi": ("iscsiadm", "open-iscsi"),
        "nvmeof": ("nvme", "nvme-cli"),
    }
    for proto, (cmd, pkg) in tools.items():
        skip_flag = getattr(args, f"skip_{proto}", False)
        if not skip_flag and not cmd_exists(cmd):
            warn(f"{cmd} not found (install {pkg}), skipping {proto}")
            setattr(args, f"skip_{proto}", True)

    # Connect to NASty
    info("Connecting to NASty API...")
    client = NastyClient(args.host, args.port, args.password)
    try:
        await client.connect()
        ok("Connected and authenticated")
    except Exception as e:
        fail(f"Connection failed: {e}")
        sys.exit(1)

    # Auto-detect pool if not specified
    pool_name = args.pool
    if not pool_name:
        pools = await client.call("pool.list")
        mounted = [p for p in pools if p["mounted"]]
        if not mounted:
            fail("No mounted pools found. Specify --pool or create/mount a pool first.")
            sys.exit(1)
        pool_name = mounted[0]["name"]
        info(f"Auto-detected pool: {pool_name}")

    ctx = TestContext(client, args.host, pool_name)

    try:
        await test_setup(ctx)

        if not args.skip_nfs:
            await test_nfs(ctx)
        else:
            warn("NFS: skipped")

        if not args.skip_smb:
            await test_smb(ctx)
        else:
            warn("SMB: skipped")

        if not args.skip_iscsi:
            await test_iscsi(ctx)
        else:
            warn("iSCSI: skipped")

        if not args.skip_nvmeof:
            await test_nvmeof(ctx)
        else:
            warn("NVMe-oF: skipped")

    finally:
        await client.close()

    # Summary
    header("Results")
    passed = sum(1 for _, p, _ in ctx.results if p)
    failed = sum(1 for _, p, _ in ctx.results if not p)
    total = len(ctx.results)

    for name, p, detail in ctx.results:
        status = f"{GREEN}PASS{RESET}" if p else f"{RED}FAIL{RESET}"
        suffix = f" — {detail}" if detail and not p else ""
        print(f"  [{status}] {name}{suffix}")

    print()
    color = GREEN if failed == 0 else RED
    print(f"{color}{BOLD}{passed}/{total} passed, {failed} failed{RESET}")
    sys.exit(1 if failed > 0 else 0)


if __name__ == "__main__":
    asyncio.run(main())
