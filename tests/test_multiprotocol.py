import asyncio
import os

from nasty.context import TestContext
from nasty.output import info
from nasty.shell import run


async def test_multiprotocol(ctx: TestContext):
    """Share one filesystem subvolume via NFS + SMB simultaneously, verify both."""
    from nasty.output import header
    header("Multi-Protocol Tests (NFS + SMB on same subvolume)")

    sv_name = f"test-multi-{ctx.tag}"
    nfs_mount = f"/tmp/nasty-test-multi-nfs-{ctx.tag}"
    smb_mount = f"/tmp/nasty-test-multi-smb-{ctx.tag}"
    nfs_share_id = None
    smb_share_id = None
    nfs_mounted = False
    smb_mounted = False
    sv = None

    try:
        # ── Create one subvolume ──────────────────────────────────
        info(f"Creating filesystem subvolume '{sv_name}'...")
        sv = await ctx.client.call("subvolume.create", {
            "pool": ctx.pool,
            "name": sv_name,
            "subvolume_type": "filesystem",
        })
        ctx.record("Multi: subvolume created", True)

        # ── Share via NFS ─────────────────────────────────────────
        info("Creating NFS share on same subvolume...")
        nfs_share = await ctx.client.call("share.nfs.create", {
            "path": sv["path"],
            "clients": [{"host": "*", "options": "rw,sync,no_subtree_check,no_root_squash"}],
        })
        nfs_share_id = nfs_share["id"]
        ctx.record("Multi: NFS share created", True)

        # ── Share via SMB ─────────────────────────────────────────
        info("Creating SMB share on same subvolume...")
        smb_share = await ctx.client.call("share.smb.create", {
            "name": sv_name,
            "path": sv["path"],
            "guest_ok": True,
        })
        smb_share_id = smb_share["id"]
        ctx.record("Multi: SMB share created", True)

        # ── Mount NFS ─────────────────────────────────────────────
        info(f"Mounting NFS at {nfs_mount}...")
        os.makedirs(nfs_mount, exist_ok=True)
        r = run(["mount", "-t", "nfs4", f"{ctx.host}:{sv['path']}", nfs_mount], check=False)
        if r.returncode != 0:
            ctx.record("Multi: NFS mount", False, r.stderr.strip())
        else:
            nfs_mounted = True
            ctx.record("Multi: NFS mount", True)

        # ── Mount SMB ─────────────────────────────────────────────
        info(f"Mounting SMB at {smb_mount}...")
        os.makedirs(smb_mount, exist_ok=True)
        r = run(["mount", "-t", "cifs", f"//{ctx.host}/{sv_name}", smb_mount,
                 "-o", "guest,vers=3.0"], check=False)
        if r.returncode != 0:
            ctx.record("Multi: SMB mount", False, r.stderr.strip())
        else:
            smb_mounted = True
            ctx.record("Multi: SMB mount", True)

        # ── Write via NFS, read via SMB ───────────────────────────
        if nfs_mounted:
            test_data = f"multi-nfs-write-{ctx.tag}"
            with open(os.path.join(nfs_mount, "cross-test.txt"), "w") as f:
                f.write(test_data)
            ctx.record("Multi: write via NFS", True)

            if smb_mounted:
                await asyncio.sleep(1)  # allow SMB cache to settle
                try:
                    with open(os.path.join(smb_mount, "cross-test.txt")) as f:
                        got = f.read()
                    ctx.record("Multi: read via SMB (NFS-written)", got == test_data,
                               "" if got == test_data else f"expected '{test_data}', got '{got}'")
                except Exception as e:
                    ctx.record("Multi: read via SMB (NFS-written)", False, str(e))

        # ── Write via SMB, read via NFS ───────────────────────────
        if smb_mounted:
            test_data2 = f"multi-smb-write-{ctx.tag}"
            with open(os.path.join(smb_mount, "cross-test2.txt"), "w") as f:
                f.write(test_data2)
            ctx.record("Multi: write via SMB", True)

            if nfs_mounted:
                # NFS may cache; drop caches by remounting or just read
                try:
                    with open(os.path.join(nfs_mount, "cross-test2.txt")) as f:
                        got = f.read()
                    ctx.record("Multi: read via NFS (SMB-written)", got == test_data2,
                               "" if got == test_data2 else f"expected '{test_data2}', got '{got}'")
                except Exception as e:
                    ctx.record("Multi: read via NFS (SMB-written)", False, str(e))

    except Exception as e:
        ctx.record("Multi: test", False, str(e))
    finally:
        if smb_mounted:
            run(["umount", smb_mount], check=False)
        if os.path.isdir(smb_mount):
            os.rmdir(smb_mount)
        if nfs_mounted:
            run(["umount", nfs_mount], check=False)
        if os.path.isdir(nfs_mount):
            os.rmdir(nfs_mount)
        if not ctx.skip_delete:
            if smb_share_id:
                try:
                    await ctx.client.call("share.smb.delete", {"id": smb_share_id})
                except Exception:
                    pass
            if nfs_share_id:
                try:
                    await ctx.client.call("share.nfs.delete", {"id": nfs_share_id})
                except Exception:
                    pass
            if sv:
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_name})
                except Exception:
                    pass
