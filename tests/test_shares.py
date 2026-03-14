#!/usr/bin/env python3
"""
NASty Integration Test Suite

Tests the full workflow:
  pool exists → create subvolumes → expose via NFS/SMB/iSCSI/NVMe-oF → mount/connect from client

Requirements (on the Linux test client):
  pip install websockets
  apt install nfs-common cifs-utils open-iscsi nvme-cli e2fsprogs  # or equivalent

Usage:
  sudo python3 test_shares.py --host 10.10.10.46
  sudo python3 test_shares.py --host 10.10.10.46 --pool mypool
  sudo python3 test_shares.py --host 10.10.10.46 --skip-nfs --skip-smb
  sudo python3 test_shares.py --host 10.10.10.46 --skip-delete   # leave subvolumes behind
  sudo python3 test_shares.py --host 10.10.10.46 --delete-only   # clean up leftovers
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
    def __init__(self, client: NastyClient, host: str, pool: str, skip_delete: bool = False):
        self.client = client
        self.host = host
        self.pool = pool
        self.skip_delete = skip_delete
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
    """Create 4 NFS shares simultaneously, mount all, write/read all, cleanup."""
    header("NFS Tests (x4 concurrent)")
    N = 4
    sv_names = [f"test-nfs{i}-{ctx.tag}" for i in range(1, N + 1)]
    mount_points = [f"/tmp/nasty-test-nfs{i}-{ctx.tag}" for i in range(1, N + 1)]
    share_ids = [None] * N
    svs = [None] * N
    mounted = [False] * N

    try:
        # Create all subvolumes and shares
        for i in range(N):
            label = f"NFS[{i+1}]"
            info(f"Creating filesystem subvolume '{sv_names[i]}'...")
            svs[i] = await ctx.client.call("subvolume.create", {
                "pool": ctx.pool,
                "name": sv_names[i],
                "subvolume_type": "filesystem",
            })
            ctx.record(f"{label}: subvolume created", True)

            info(f"Creating NFS share for {sv_names[i]}...")
            share = await ctx.client.call("share.nfs.create", {
                "path": svs[i]["path"],
                "clients": [{"host": "*", "options": "rw,sync,no_subtree_check,no_root_squash"}],
            })
            share_ids[i] = share["id"]
            ctx.record(f"{label}: share created", True)

        # Mount all simultaneously
        for i in range(N):
            label = f"NFS[{i+1}]"
            info(f"Mounting NFS share at {mount_points[i]}...")
            os.makedirs(mount_points[i], exist_ok=True)
            r = run(["mount", "-t", "nfs4", f"{ctx.host}:{svs[i]['path']}", mount_points[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: mount", False, r.stderr.strip())
            else:
                mounted[i] = True
                ctx.record(f"{label}: mount", True)

        # Write to all mounted shares
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"NFS[{i+1}]"
            test_data = f"nasty-nfs-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "w") as f:
                f.write(test_data)
            ctx.record(f"{label}: write", True)

        # Read back from all mounted shares
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"NFS[{i+1}]"
            test_data = f"nasty-nfs-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "r") as f:
                read_back = f.read()
            if read_back == test_data:
                ctx.record(f"{label}: read/verify", True)
            else:
                ctx.record(f"{label}: read/verify", False, f"expected '{test_data}', got '{read_back}'")

        # Snapshot all subvolumes
        snap_names = [f"snap-nfs{i+1}-{ctx.tag}" for i in range(N)]
        for i in range(N):
            label = f"NFS[{i+1}]"
            info(f"Creating snapshot '{snap_names[i]}' of '{sv_names[i]}'...")
            try:
                snap = await ctx.client.call("snapshot.create", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                    "read_only": True,
                })
                ctx.record(f"{label}: snapshot created", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot created", False, str(e))

        # Verify snapshots appear in listing
        snapshots = await ctx.client.call("snapshot.list", {"pool": ctx.pool})
        for i in range(N):
            label = f"NFS[{i+1}]"
            found = any(s["name"] == snap_names[i] and s["subvolume"] == sv_names[i] for s in snapshots)
            ctx.record(f"{label}: snapshot listed", found,
                       "" if found else f"snapshot '{snap_names[i]}' not found in listing")

        # Delete snapshots
        for i in range(N):
            label = f"NFS[{i+1}]"
            try:
                await ctx.client.call("snapshot.delete", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                })
                ctx.record(f"{label}: snapshot deleted", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot deleted", False, str(e))

    except Exception as e:
        ctx.record("NFS: test", False, str(e))
    finally:
        for i in range(N):
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if not ctx.skip_delete:
                if share_ids[i]:
                    try:
                        await ctx.client.call("share.nfs.delete", {"id": share_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass


async def test_smb(ctx: TestContext):
    """Create 4 SMB shares simultaneously, mount all, write/read all, cleanup."""
    header("SMB Tests (x4 concurrent)")
    N = 4
    sv_names = [f"test-smb{i}-{ctx.tag}" for i in range(1, N + 1)]
    share_names = [f"tsmb{i}{ctx.tag}" for i in range(1, N + 1)]
    mount_points = [f"/tmp/nasty-test-smb{i}-{ctx.tag}" for i in range(1, N + 1)]
    share_ids = [None] * N
    svs = [None] * N
    mounted = [False] * N

    try:
        # Create all subvolumes and shares
        for i in range(N):
            label = f"SMB[{i+1}]"
            info(f"Creating filesystem subvolume '{sv_names[i]}'...")
            svs[i] = await ctx.client.call("subvolume.create", {
                "pool": ctx.pool,
                "name": sv_names[i],
                "subvolume_type": "filesystem",
            })
            ctx.record(f"{label}: subvolume created", True)

            info(f"Creating SMB share '{share_names[i]}'...")
            share = await ctx.client.call("share.smb.create", {
                "name": share_names[i],
                "path": svs[i]["path"],
                "guest_ok": True,
                "browseable": True,
            })
            share_ids[i] = share["id"]
            ctx.record(f"{label}: share created", True)

        # Pause for Samba to reload config
        await asyncio.sleep(3)

        # Mount all simultaneously
        for i in range(N):
            label = f"SMB[{i+1}]"
            info(f"Mounting SMB share at {mount_points[i]}...")
            os.makedirs(mount_points[i], exist_ok=True)
            r = run(
                ["mount", "-t", "cifs", f"//{ctx.host}/{share_names[i]}", mount_points[i],
                 "-o", "guest,vers=3.0"],
                check=False,
            )
            if r.returncode != 0:
                ctx.record(f"{label}: mount", False, r.stderr.strip())
            else:
                mounted[i] = True
                ctx.record(f"{label}: mount", True)

        # Write to all mounted shares
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"SMB[{i+1}]"
            test_data = f"nasty-smb-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "w") as f:
                f.write(test_data)
            ctx.record(f"{label}: write", True)

        # Read back from all mounted shares
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"SMB[{i+1}]"
            test_data = f"nasty-smb-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "r") as f:
                read_back = f.read()
            if read_back == test_data:
                ctx.record(f"{label}: read/verify", True)
            else:
                ctx.record(f"{label}: read/verify", False, f"expected '{test_data}', got '{read_back}'")

        # Snapshot all subvolumes
        snap_names = [f"snap-smb{i+1}-{ctx.tag}" for i in range(N)]
        for i in range(N):
            label = f"SMB[{i+1}]"
            info(f"Creating snapshot '{snap_names[i]}' of '{sv_names[i]}'...")
            try:
                await ctx.client.call("snapshot.create", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                    "read_only": True,
                })
                ctx.record(f"{label}: snapshot created", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot created", False, str(e))

        # Verify snapshots appear in listing
        snapshots = await ctx.client.call("snapshot.list", {"pool": ctx.pool})
        for i in range(N):
            label = f"SMB[{i+1}]"
            found = any(s["name"] == snap_names[i] and s["subvolume"] == sv_names[i] for s in snapshots)
            ctx.record(f"{label}: snapshot listed", found,
                       "" if found else f"snapshot '{snap_names[i]}' not found in listing")

        # Delete snapshots
        for i in range(N):
            label = f"SMB[{i+1}]"
            try:
                await ctx.client.call("snapshot.delete", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                })
                ctx.record(f"{label}: snapshot deleted", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot deleted", False, str(e))

    except Exception as e:
        ctx.record("SMB: test", False, str(e))
    finally:
        for i in range(N):
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if not ctx.skip_delete:
                if share_ids[i]:
                    try:
                        await ctx.client.call("share.smb.delete", {"id": share_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass


def find_iscsi_device(iqn: str) -> str | None:
    """Find the /dev/sdX device for a logged-in iSCSI target by IQN."""
    r = run(["iscsiadm", "-m", "session", "-P", "3"], check=False)
    if r.returncode != 0:
        return None
    # Parse session output: find the block after our IQN, then look for "Attached scsi disk"
    in_target = False
    for line in r.stdout.splitlines():
        if iqn in line:
            in_target = True
        elif in_target and "Target:" in line:
            break  # next target
        elif in_target and "Attached scsi disk" in line:
            # Line looks like: "Attached scsi disk sda    State: running"
            parts = line.strip().split()
            idx = parts.index("disk") + 1 if "disk" in parts else -1
            if idx > 0 and idx < len(parts):
                return f"/dev/{parts[idx]}"
    return None


async def test_iscsi(ctx: TestContext):
    """Create 4 iSCSI targets simultaneously, login all, format+write/read all, cleanup."""
    header("iSCSI Tests (x4 concurrent)")
    N = 4
    sv_names = [f"test-iscsi{i}-{ctx.tag}" for i in range(1, N + 1)]
    target_names = [f"test-iscsi{i}-{ctx.tag}" for i in range(1, N + 1)]
    iqns = [f"iqn.2024-01.com.nasty:{n}" for n in target_names]
    mount_points = [f"/tmp/nasty-test-iscsi{i}-{ctx.tag}" for i in range(1, N + 1)]
    target_ids = [None] * N
    logged_in = [False] * N
    mounted = [False] * N
    devices = [None] * N

    try:
        # Create all block subvolumes and iSCSI targets
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            info(f"Creating block subvolume '{sv_names[i]}' (64 MiB)...")
            sv = await ctx.client.call("subvolume.create", {
                "pool": ctx.pool,
                "name": sv_names[i],
                "subvolume_type": "block",
                "volsize_bytes": 64 * 1024 * 1024,
            })
            block_dev = sv.get("block_device")
            if not block_dev:
                ctx.record(f"{label}: block device", False, "no block_device returned")
                return
            ctx.record(f"{label}: block subvolume created", True)

            info(f"Creating iSCSI target for {sv_names[i]}...")
            target = await ctx.client.call("share.iscsi.create_quick", {
                "name": target_names[i],
                "device_path": block_dev,
            })
            target_ids[i] = target["id"]
            ctx.record(f"{label}: target created", True)

        # Discover and login all
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            info(f"Discovering iSCSI targets on {ctx.host}...")
            r = run(
                ["iscsiadm", "-m", "discovery", "-t", "sendtargets", "-p", ctx.host],
                check=False,
            )
            if r.returncode != 0:
                ctx.record(f"{label}: discovery", False, r.stderr.strip())
                continue
            if iqns[i] not in r.stdout:
                ctx.record(f"{label}: discovery", False, f"IQN {iqns[i]} not in discovery output")
                continue
            ctx.record(f"{label}: discovery", True)

            info(f"Logging in to {iqns[i]}...")
            r = run(
                ["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "--login"],
                check=False,
            )
            if r.returncode != 0:
                ctx.record(f"{label}: login", False, r.stderr.strip())
                continue
            logged_in[i] = True
            ctx.record(f"{label}: login", True)

        # Wait for all devices to appear
        await asyncio.sleep(3)

        # Find devices and format+mount+write all
        for i in range(N):
            if not logged_in[i]:
                continue
            label = f"iSCSI[{i+1}]"

            dev = find_iscsi_device(iqns[i])
            if not dev:
                ctx.record(f"{label}: device attached", False, "no scsi disk found for IQN")
                continue
            devices[i] = dev
            ctx.record(f"{label}: device attached", True)

            # Format with ext4
            info(f"Formatting {dev} with ext4...")
            r = run(["mkfs.ext4", "-F", "-q", dev], check=False, timeout=60)
            if r.returncode != 0:
                ctx.record(f"{label}: mkfs.ext4", False, r.stderr.strip())
                continue
            ctx.record(f"{label}: mkfs.ext4", True)

            # Mount
            os.makedirs(mount_points[i], exist_ok=True)
            r = run(["mount", dev, mount_points[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: mount", False, r.stderr.strip())
                continue
            mounted[i] = True
            ctx.record(f"{label}: mount", True)

        # Write to all mounted devices
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"iSCSI[{i+1}]"
            test_data = f"nasty-iscsi-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "w") as f:
                f.write(test_data)
            ctx.record(f"{label}: write", True)

        # Read back from all mounted devices
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"iSCSI[{i+1}]"
            test_data = f"nasty-iscsi-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "r") as f:
                read_back = f.read()
            if read_back == test_data:
                ctx.record(f"{label}: read/verify", True)
            else:
                ctx.record(f"{label}: read/verify", False, f"expected '{test_data}', got '{read_back}'")

        # Snapshot all subvolumes
        snap_names = [f"snap-iscsi{i+1}-{ctx.tag}" for i in range(N)]
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            info(f"Creating snapshot '{snap_names[i]}' of '{sv_names[i]}'...")
            try:
                await ctx.client.call("snapshot.create", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                    "read_only": True,
                })
                ctx.record(f"{label}: snapshot created", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot created", False, str(e))

        # Verify snapshots appear in listing
        snapshots = await ctx.client.call("snapshot.list", {"pool": ctx.pool})
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            found = any(s["name"] == snap_names[i] and s["subvolume"] == sv_names[i] for s in snapshots)
            ctx.record(f"{label}: snapshot listed", found,
                       "" if found else f"snapshot '{snap_names[i]}' not found in listing")

        # Delete snapshots
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            try:
                await ctx.client.call("snapshot.delete", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                })
                ctx.record(f"{label}: snapshot deleted", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot deleted", False, str(e))

    except Exception as e:
        ctx.record("iSCSI: test", False, str(e))
    finally:
        for i in range(N):
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if logged_in[i]:
                run(
                    ["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "--logout"],
                    check=False,
                )
            run(
                ["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "-o", "delete"],
                check=False,
            )
            if not ctx.skip_delete:
                if target_ids[i]:
                    try:
                        await ctx.client.call("share.iscsi.delete", {"id": target_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.detach", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass


def find_nvme_device(nqn: str) -> str | None:
    """Find the /dev/nvmeXnY device for a connected NVMe-oF subsystem by NQN."""
    r = run(["nvme", "list-subsys", "-o", "json"], check=False)
    if r.returncode != 0:
        return None
    try:
        data = json.loads(r.stdout)
        # nvme list-subsys JSON structure varies; search for our NQN
        subsystems = data if isinstance(data, list) else data.get("Subsystems", [])
        for subsys in subsystems:
            if subsys.get("NQN") == nqn or subsys.get("SubsystemNQN") == nqn:
                # Find first path with a namespace device
                for path in subsys.get("Paths", []):
                    name = path.get("Name", "")
                    if name:
                        # Name is like "nvme1" — device is /dev/nvme1n1
                        return f"/dev/{name}n1"
                # Alternative structure: Namespaces directly
                for ns in subsys.get("Namespaces", []):
                    name = ns.get("NameSpace") or ns.get("Name", "")
                    if name:
                        return f"/dev/{name}"
    except (json.JSONDecodeError, KeyError):
        pass
    # Fallback: scan /dev for nvme devices and check their subsysnqn
    import glob
    for dev in sorted(glob.glob("/dev/nvme*n1")):
        r2 = run(["nvme", "id-ctrl", dev, "-o", "json"], check=False)
        if r2.returncode == 0:
            try:
                ctrl = json.loads(r2.stdout)
                if ctrl.get("subnqn") == nqn:
                    return dev
            except (json.JSONDecodeError, KeyError):
                pass
    return None


async def test_nvmeof(ctx: TestContext):
    """Create 4 NVMe-oF shares simultaneously, connect all, format+write/read all, cleanup."""
    header("NVMe-oF Tests (x4 concurrent)")
    N = 4
    sv_names = [f"test-nvme{i}-{ctx.tag}" for i in range(1, N + 1)]
    subsys_names = [f"test-nvme{i}-{ctx.tag}" for i in range(1, N + 1)]
    nqns = [f"nqn.2024-01.com.nasty:{n}" for n in subsys_names]
    mount_points = [f"/tmp/nasty-test-nvme{i}-{ctx.tag}" for i in range(1, N + 1)]
    subsys_ids = [None] * N
    connected = [False] * N
    mounted = [False] * N
    devices = [None] * N

    try:
        # Create all block subvolumes and NVMe-oF shares
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            info(f"Creating block subvolume '{sv_names[i]}' (64 MiB)...")
            sv = await ctx.client.call("subvolume.create", {
                "pool": ctx.pool,
                "name": sv_names[i],
                "subvolume_type": "block",
                "volsize_bytes": 64 * 1024 * 1024,
            })
            block_dev = sv.get("block_device")
            if not block_dev:
                ctx.record(f"{label}: block device", False, "no block_device returned")
                return
            ctx.record(f"{label}: block subvolume created", True)

            info(f"Creating NVMe-oF share for {sv_names[i]}...")
            subsys = await ctx.client.call("share.nvmeof.create_quick", {
                "name": subsys_names[i],
                "device_path": block_dev,
            })
            subsys_ids[i] = subsys["id"]
            ctx.record(f"{label}: share created", True)

        # Connect all
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            info(f"Connecting to NVMe-oF target {nqns[i]}...")
            r = run(
                ["nvme", "connect", "-t", "tcp", "-n", nqns[i],
                 "-a", ctx.host, "-s", "4420"],
                check=False,
            )
            if r.returncode != 0:
                ctx.record(f"{label}: connect", False, r.stderr.strip())
                continue
            connected[i] = True
            ctx.record(f"{label}: connect", True)

        # Wait for all devices to appear
        await asyncio.sleep(3)

        # Find devices, format, mount all
        for i in range(N):
            if not connected[i]:
                continue
            label = f"NVMe-oF[{i+1}]"

            dev = find_nvme_device(nqns[i])
            if not dev:
                ctx.record(f"{label}: device visible", False, "NQN not found in nvme list-subsys")
                continue
            devices[i] = dev
            ctx.record(f"{label}: device visible", True)

            # Format with ext4
            info(f"Formatting {dev} with ext4...")
            r = run(["mkfs.ext4", "-F", "-q", dev], check=False, timeout=60)
            if r.returncode != 0:
                ctx.record(f"{label}: mkfs.ext4", False, r.stderr.strip())
                continue
            ctx.record(f"{label}: mkfs.ext4", True)

            # Mount
            os.makedirs(mount_points[i], exist_ok=True)
            r = run(["mount", dev, mount_points[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: mount", False, r.stderr.strip())
                continue
            mounted[i] = True
            ctx.record(f"{label}: mount", True)

        # Write to all mounted devices
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"NVMe-oF[{i+1}]"
            test_data = f"nasty-nvme-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "w") as f:
                f.write(test_data)
            ctx.record(f"{label}: write", True)

        # Read back from all mounted devices
        for i in range(N):
            if not mounted[i]:
                continue
            label = f"NVMe-oF[{i+1}]"
            test_data = f"nasty-nvme-test{i+1}-{ctx.tag}"
            test_file = os.path.join(mount_points[i], "testfile.txt")
            with open(test_file, "r") as f:
                read_back = f.read()
            if read_back == test_data:
                ctx.record(f"{label}: read/verify", True)
            else:
                ctx.record(f"{label}: read/verify", False, f"expected '{test_data}', got '{read_back}'")

        # Snapshot all subvolumes
        snap_names = [f"snap-nvme{i+1}-{ctx.tag}" for i in range(N)]
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            info(f"Creating snapshot '{snap_names[i]}' of '{sv_names[i]}'...")
            try:
                await ctx.client.call("snapshot.create", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                    "read_only": True,
                })
                ctx.record(f"{label}: snapshot created", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot created", False, str(e))

        # Verify snapshots appear in listing
        snapshots = await ctx.client.call("snapshot.list", {"pool": ctx.pool})
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            found = any(s["name"] == snap_names[i] and s["subvolume"] == sv_names[i] for s in snapshots)
            ctx.record(f"{label}: snapshot listed", found,
                       "" if found else f"snapshot '{snap_names[i]}' not found in listing")

        # Delete snapshots
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            try:
                await ctx.client.call("snapshot.delete", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "name": snap_names[i],
                })
                ctx.record(f"{label}: snapshot deleted", True)
            except Exception as e:
                ctx.record(f"{label}: snapshot deleted", False, str(e))

    except Exception as e:
        ctx.record("NVMe-oF: test", False, str(e))
    finally:
        for i in range(N):
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if connected[i]:
                run(["nvme", "disconnect", "-n", nqns[i]], check=False)
            if not ctx.skip_delete:
                if subsys_ids[i]:
                    try:
                        await ctx.client.call("share.nvmeof.delete", {"id": subsys_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.detach", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass


# ── Leftover cleanup ──────────────────────────────────────────────

async def delete_leftovers(client: NastyClient, pool_name: str):
    """Delete all test-* subvolumes and their shares left behind by --skip-delete runs."""
    header("Cleanup: Deleting test leftovers")
    TEST_PREFIX = "test-"

    subvolumes = await client.call("subvolume.list", {"pool": pool_name})
    test_svs = [sv for sv in subvolumes if sv["name"].startswith(TEST_PREFIX)]
    if not test_svs:
        info("No test subvolumes found.")
        return

    test_paths = {sv["path"] for sv in test_svs}

    for proto, list_method, delete_method, path_key in [
        ("NFS",     "share.nfs.list",    "share.nfs.delete",    "path"),
        ("SMB",     "share.smb.list",    "share.smb.delete",    "path"),
        ("iSCSI",   "share.iscsi.list",  "share.iscsi.delete",  None),
        ("NVMe-oF", "share.nvmeof.list", "share.nvmeof.delete", None),
    ]:
        try:
            shares = await client.call(list_method)
            for share in shares:
                match = (
                    share.get("path") in test_paths if path_key
                    else share.get("name", "").startswith(TEST_PREFIX)
                )
                if match:
                    info(f"Deleting {proto} share '{share.get('name') or share['id']}'...")
                    await client.call(delete_method, {"id": share["id"]})
                    ok("Deleted")
        except Exception as e:
            warn(f"{proto} share cleanup: {e}")

    for sv in test_svs:
        info(f"Deleting subvolume '{sv['name']}'...")
        if sv.get("subvolume_type") == "block":
            try:
                await client.call("subvolume.detach", {"pool": pool_name, "name": sv["name"]})
            except Exception:
                pass
        try:
            await client.call("subvolume.delete", {"pool": pool_name, "name": sv["name"]})
            ok(f"Deleted '{sv['name']}'")
        except Exception as e:
            warn(f"Delete '{sv['name']}': {e}")


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
    parser.add_argument("--skip-delete", action="store_true",
                        help="Skip server-side deletions after tests (leave subvolumes/shares behind)")
    parser.add_argument("--delete-only", action="store_true",
                        help="Delete all test-* leftovers from a prior --skip-delete run, then exit")
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

    if args.delete_only:
        try:
            await delete_leftovers(client, pool_name)
        finally:
            await client.close()
        return

    ctx = TestContext(client, args.host, pool_name, skip_delete=args.skip_delete)
    if args.skip_delete:
        warn("--skip-delete: subvolumes and shares will NOT be deleted after tests")

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
