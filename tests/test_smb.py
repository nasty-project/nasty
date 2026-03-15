import asyncio
import os

from nasty.context import TestContext
from nasty.output import info
from nasty.shell import run

N = 5  # subvolumes per run
S = 2  # snapshots per subvolume


async def test_smb(ctx: TestContext):
    """Create N SMB shares, mount all, write/read, take S snapshots each, cleanup."""
    from nasty.output import header
    header(f"SMB Tests (x{N} concurrent)")

    sv_names     = [f"test-smb{i}-{ctx.tag}"        for i in range(1, N + 1)]
    share_names  = [f"test-smb{i}-{ctx.tag}"        for i in range(1, N + 1)]
    mount_points = [f"/tmp/nasty-test-smb{i}-{ctx.tag}" for i in range(1, N + 1)]
    share_ids        = [None] * N
    svs              = [None] * N
    mounted          = [False] * N
    clone_names      = [f"test-smb{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_share_names = [f"test-smb{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_share_ids  = [None] * N
    clone_mounts     = [f"/tmp/nasty-test-smb{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_mounted    = [False] * N

    try:
        if not ctx.remount:
            # ── Create subvolumes + shares ────────────────────────
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

        # ── Mount ─────────────────────────────────────────────────
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

        # ── Write ─────────────────────────────────────────────────
        if not ctx.remount:
            for i in range(N):
                if not mounted[i]:
                    continue
                test_data = f"nasty-smb-test{i+1}-{ctx.tag}"
                with open(os.path.join(mount_points[i], "testfile.txt"), "w") as f:
                    f.write(test_data)
                ctx.record(f"SMB[{i+1}]: write", True)

        # ── Read/verify ───────────────────────────────────────────
        for i in range(N):
            if not mounted[i]:
                continue
            expected = f"nasty-smb-test{i+1}-{ctx.tag}"
            with open(os.path.join(mount_points[i], "testfile.txt")) as f:
                got = f.read()
            ctx.record(f"SMB[{i+1}]: read/verify", got == expected,
                       "" if got == expected else f"expected '{expected}', got '{got}'")

        if ctx.remount:
            return

        # ── Snapshots ─────────────────────────────────────────────
        snap_names = [[f"snap-smb{i+1}-s{j+1}-{ctx.tag}" for j in range(S)] for i in range(N)]
        for i in range(N):
            for j in range(S):
                label = f"SMB[{i+1}]"
                info(f"Creating snapshot '{snap_names[i][j]}' of '{sv_names[i]}'...")
                try:
                    await ctx.client.call("snapshot.create", {
                        "pool": ctx.pool,
                        "subvolume": sv_names[i],
                        "name": snap_names[i][j],
                        "read_only": True,
                    })
                    ctx.record(f"{label}: snapshot {j+1} created", True)
                except Exception as e:
                    ctx.record(f"{label}: snapshot {j+1} created", False, str(e))

        snapshots = await ctx.client.call("snapshot.list", {"pool": ctx.pool})
        for i in range(N):
            for j in range(S):
                found = any(s["name"] == snap_names[i][j] and s["subvolume"] == sv_names[i]
                            for s in snapshots)
                ctx.record(f"SMB[{i+1}]: snapshot {j+1} listed", found,
                           "" if found else f"'{snap_names[i][j]}' not found")

        # ── Clone ─────────────────────────────────────────────────
        await asyncio.sleep(1)

        for i in range(N):
            label = f"SMB[{i+1}] clone"
            info(f"Cloning '{snap_names[i][0]}' → '{clone_names[i]}'...")
            try:
                clone = await ctx.client.call("snapshot.clone", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "snapshot": snap_names[i][0],
                    "new_name": clone_names[i],
                })
                ctx.record(f"{label}: created", True)
            except Exception as e:
                ctx.record(f"{label}: created", False, str(e))
                continue

            try:
                share = await ctx.client.call("share.smb.create", {
                    "name": clone_share_names[i],
                    "path": clone["path"],
                    "guest_ok": True,
                    "browseable": True,
                })
                clone_share_ids[i] = share["id"]
            except Exception as e:
                ctx.record(f"{label}: read/verify", False, f"share create: {e}")
                continue

        await asyncio.sleep(3)

        for i in range(N):
            if clone_share_ids[i] is None:
                continue
            label = f"SMB[{i+1}] clone"
            os.makedirs(clone_mounts[i], exist_ok=True)
            r = run(
                ["mount", "-t", "cifs", f"//{ctx.host}/{clone_share_names[i]}", clone_mounts[i],
                 "-o", "guest,vers=3.0"],
                check=False,
            )
            if r.returncode != 0:
                ctx.record(f"{label}: read/verify", False, f"mount: {r.stderr.strip()}")
                continue
            clone_mounted[i] = True

            expected = f"nasty-smb-test{i+1}-{ctx.tag}"
            try:
                with open(os.path.join(clone_mounts[i], "testfile.txt")) as f:
                    got = f.read()
                ctx.record(f"{label}: read/verify", got == expected,
                           "" if got == expected else f"expected '{expected}', got '{got}'")
            except Exception as e:
                ctx.record(f"{label}: read/verify", False, str(e))

        if not ctx.skip_delete:
            for i in range(N):
                for j in range(S):
                    try:
                        await ctx.client.call("snapshot.delete", {
                            "pool": ctx.pool, "subvolume": sv_names[i], "name": snap_names[i][j],
                        })
                        ctx.record(f"SMB[{i+1}]: snapshot {j+1} deleted", True)
                    except Exception as e:
                        ctx.record(f"SMB[{i+1}]: snapshot {j+1} deleted", False, str(e))

    except Exception as e:
        ctx.record("SMB: test", False, str(e))
    finally:
        for i in range(N):
            if clone_mounted[i]:
                run(["umount", clone_mounts[i]], check=False)
            if os.path.isdir(clone_mounts[i]):
                os.rmdir(clone_mounts[i])
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if not ctx.skip_delete:
                if clone_share_ids[i]:
                    try:
                        await ctx.client.call("share.smb.delete", {"id": clone_share_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": clone_names[i]})
                except Exception:
                    pass
                if share_ids[i]:
                    try:
                        await ctx.client.call("share.smb.delete", {"id": share_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": sv_names[i]})
                except Exception:
                    pass
