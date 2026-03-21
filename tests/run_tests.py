#!/usr/bin/env python3
"""
NASty Integration Test Runner

Usage:
  sudo python3 run_tests.py --host 10.10.10.46
  sudo python3 run_tests.py --host 10.10.10.46 --pool tank --skip-nvmeof
  sudo python3 run_tests.py --host 10.10.10.46 --skip-delete
  sudo python3 run_tests.py --host 10.10.10.46 --delete-only
"""

import argparse
import asyncio
import os
import sys

from nasty.client import NastyClient
from nasty.context import TestContext
from nasty.output import GREEN, RED, BOLD, RESET, info, ok, fail, warn, header
from nasty.shell import cmd_exists

from test_nfs import test_nfs
from test_smb import test_smb
from test_iscsi import test_iscsi
from test_nvmeof import test_nvmeof
from test_subvolume import test_subvolume
from test_snapshots import test_snapshots
from test_storage import test_storage
from test_multiprotocol import test_multiprotocol
from test_cleanup import delete_leftovers


async def test_setup(ctx: TestContext):
    header("Setup")

    info(f"Verifying pool '{ctx.pool}' exists...")
    pools = await ctx.client.call("pool.list")
    pool = next((p for p in pools if p["name"] == ctx.pool), None)
    if not pool:
        fail(f"Pool '{ctx.pool}' not found. Available: {[p['name'] for p in pools]}")
        sys.exit(1)
    if not pool["mounted"]:
        info(f"Mounting pool '{ctx.pool}'...")
        await ctx.client.call("pool.mount", {"name": ctx.pool})
    ok(f"Pool '{ctx.pool}' is mounted")

    info("Enabling protocols...")
    for proto in ["nfs", "smb", "iscsi", "nvmeof"]:
        try:
            await ctx.client.call("service.protocol.enable", {"name": proto})
            ok(f"Enabled {proto}")
        except Exception as e:
            warn(f"Enable {proto}: {e}")

    await asyncio.sleep(2)


async def main():
    parser = argparse.ArgumentParser(description="NASty integration test suite")
    parser.add_argument("--host",        required=True,       help="NASty appliance IP/hostname")
    parser.add_argument("--port",        type=int, default=443, help="WebUI HTTPS port (default 443)")
    parser.add_argument("--password",    default="admin",     help="Admin password (default 'admin')")
    parser.add_argument("--pool",        default=None,        help="Pool name (auto-detected if omitted; created if not found and --create-pool is set)")
    parser.add_argument("--create-pool", action="store_true", help="Auto-create the pool using available unmounted block devices if it does not exist")
    parser.add_argument("--skip-nfs",       action="store_true")
    parser.add_argument("--skip-smb",       action="store_true")
    parser.add_argument("--skip-iscsi",     action="store_true")
    parser.add_argument("--skip-nvmeof",    action="store_true")
    parser.add_argument("--skip-subvolume", action="store_true")
    parser.add_argument("--skip-snapshots", action="store_true")
    parser.add_argument("--skip-storage",   action="store_true")
    parser.add_argument("--skip-delete", action="store_true",
                        help="Skip server-side deletions (leave subvolumes/shares behind)")
    parser.add_argument("--delete-only", action="store_true",
                        help="Delete all test-* leftovers from a prior --skip-delete run, then exit")
    parser.add_argument("--tag", default=None,
                        help="Reuse a specific tag from a prior run (e.g. from --skip-delete)")
    parser.add_argument("--remount", action="store_true",
                        help="Skip creation/writes, mount existing shares and verify data only (use with --tag)")
    args = parser.parse_args()

    if os.geteuid() != 0:
        print(f"{RED}ERROR:{RESET} This test must be run as root (needs mount/iscsi/nvme)")
        sys.exit(1)

    header("NASty Integration Test Suite")
    info(f"Target: {args.host}:{args.port}")

    # Warn and auto-skip if client tools are missing
    for proto, (cmd, pkg) in {
        "nfs":    ("mount.nfs",  "nfs-common"),
        "smb":    ("mount.cifs", "cifs-utils"),
        "iscsi":  ("iscsiadm",   "open-iscsi"),
        "nvmeof": ("nvme",       "nvme-cli"),
    }.items():
        if not getattr(args, f"skip_{proto}") and not cmd_exists(cmd):
            warn(f"{cmd} not found (install {pkg}), skipping {proto}")
            setattr(args, f"skip_{proto}", True)

    info("Connecting to NASty API...")
    client = NastyClient(args.host, args.port, args.password)
    try:
        await client.connect()
        ok("Connected and authenticated")
    except Exception as e:
        fail(f"Connection failed: {e}")
        sys.exit(1)

    pool_name = args.pool
    if not pool_name:
        pools = await client.call("pool.list")
        mounted = [p for p in pools if p["mounted"]]
        if not mounted:
            fail("No mounted pools found. Specify --pool or mount a pool first.")
            await client.close()
            sys.exit(1)
        pool_name = mounted[0]["name"]
        info(f"Auto-detected pool: {pool_name}")
    elif args.create_pool:
        # Create the pool if it doesn't already exist
        pools = await client.call("pool.list")
        existing = next((p for p in pools if p["name"] == pool_name), None)
        if not existing:
            info(f"Pool '{pool_name}' not found — discovering available devices...")
            devices = await client.call("device.list")
            available = [d for d in devices if not d.get("in_use")]
            if not available:
                fail("No available (unused) block devices found to create pool.")
                await client.close()
                sys.exit(1)
            device_specs = [{"path": d["path"]} for d in available]
            info(f"Creating pool '{pool_name}' on {[d['path'] for d in device_specs]}...")
            try:
                await client.call("pool.create", {"name": pool_name, "devices": device_specs})
                ok(f"Pool '{pool_name}' created")
            except Exception as e:
                fail(f"Failed to create pool '{pool_name}': {e}")
                await client.close()
                sys.exit(1)
        else:
            info(f"Pool '{pool_name}' already exists")

    if args.delete_only:
        try:
            await delete_leftovers(client, pool_name)
        finally:
            await client.close()
        return

    ctx = TestContext(client, args.host, pool_name, skip_delete=args.skip_delete,
                     tag=args.tag, remount=args.remount)

    info(f"Test tag: {ctx.tag}  (reuse with --tag {ctx.tag} --remount)")
    if args.remount:
        if not args.tag:
            warn("--remount without --tag: using freshly generated tag (nothing will be found)")
        warn("--remount: skipping creation, mounting existing shares only")
    elif args.skip_delete:
        warn("--skip-delete: subvolumes and shares will NOT be deleted after tests")

    try:
        await test_setup(ctx)

        if not args.skip_subvolume and not args.remount: await test_subvolume(ctx)
        else:                       warn("Subvolume: skipped")

        if not args.skip_snapshots and not args.remount: await test_snapshots(ctx)
        else:                       warn("Snapshots: skipped")

        if not args.skip_storage and not args.remount:   await test_storage(ctx)
        else:                       warn("Storage: skipped")

        if not args.skip_nfs:    await test_nfs(ctx)
        else:                    warn("NFS: skipped")

        if not args.skip_smb:    await test_smb(ctx)
        else:                    warn("SMB: skipped")

        if not args.skip_nfs and not args.skip_smb and not args.remount:
            await test_multiprotocol(ctx)
        else:                    warn("Multi-protocol: skipped")

        if not args.skip_iscsi:  await test_iscsi(ctx)
        else:                    warn("iSCSI: skipped")

        if not args.skip_nvmeof: await test_nvmeof(ctx)
        else:                    warn("NVMe-oF: skipped")

    finally:
        await client.close()

    header("Results")
    passed = sum(1 for _, p, _ in ctx.results if p)
    failed = sum(1 for _, p, _ in ctx.results if not p)

    for name, p, detail in ctx.results:
        status = f"{GREEN}PASS{RESET}" if p else f"{RED}FAIL{RESET}"
        suffix = f" — {detail}" if detail and not p else ""
        print(f"  [{status}] {name}{suffix}")

    print()
    color = GREEN if failed == 0 else RED
    print(f"{color}{BOLD}{passed}/{len(ctx.results)} passed, {failed} failed{RESET}")
    sys.exit(1 if failed > 0 else 0)


if __name__ == "__main__":
    asyncio.run(main())
