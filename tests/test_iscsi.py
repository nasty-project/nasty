import asyncio
import os

from nasty.context import TestContext
from nasty.output import info
from nasty.shell import run

N = 5  # subvolumes per run
S = 2  # snapshots per subvolume


def find_iscsi_device(iqn: str) -> str | None:
    """Return /dev/sdX for a logged-in iSCSI target by IQN."""
    r = run(["iscsiadm", "-m", "session", "-P", "3"], check=False)
    if r.returncode != 0:
        return None
    in_target = False
    for line in r.stdout.splitlines():
        if iqn in line:
            in_target = True
        elif in_target and "Target:" in line:
            break
        elif in_target and "Attached scsi disk" in line:
            parts = line.strip().split()
            idx = parts.index("disk") + 1 if "disk" in parts else -1
            if 0 < idx < len(parts):
                return f"/dev/{parts[idx]}"
    return None


async def test_iscsi(ctx: TestContext):
    """Create N iSCSI targets, login all, format+write/read, take S snapshots each, cleanup."""
    from nasty.output import header
    header(f"iSCSI Tests (x{N} concurrent)")

    sv_names     = [f"test-iscsi{i}-{ctx.tag}"        for i in range(1, N + 1)]
    target_names = [f"test-iscsi{i}-{ctx.tag}"        for i in range(1, N + 1)]
    iqns         = [f"iqn.2137-01.com.nasty:{n}"       for n in target_names]
    mount_points = [f"/tmp/nasty-test-iscsi{i}-{ctx.tag}" for i in range(1, N + 1)]
    target_ids      = [None] * N
    logged_in       = [False] * N
    mounted         = [False] * N
    devices         = [None] * N
    clone_sv_names  = [f"test-iscsi{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_iqns      = [f"iqn.2137-01.com.nasty:test-iscsi{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_target_ids = [None] * N
    clone_mounts    = [f"/tmp/nasty-test-iscsi{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_connected = [False] * N
    clone_mounted   = [False] * N

    try:
        if not ctx.remount:
            # ── Create block subvolumes + targets ─────────────────
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

        # ── Discovery + login ─────────────────────────────────────
        for i in range(N):
            label = f"iSCSI[{i+1}]"
            info(f"Discovering iSCSI targets on {ctx.host}...")
            r = run(["iscsiadm", "-m", "discovery", "-t", "sendtargets", "-p", ctx.host], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: discovery", False, r.stderr.strip())
                continue
            if iqns[i] not in r.stdout:
                ctx.record(f"{label}: discovery", False, f"IQN {iqns[i]} not in output")
                continue
            ctx.record(f"{label}: discovery", True)

            info(f"Logging in to {iqns[i]}...")
            r = run(["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "--login"],
                    check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: login", False, r.stderr.strip())
                continue
            logged_in[i] = True
            ctx.record(f"{label}: login", True)

        await asyncio.sleep(3)

        # ── Format + mount + write/read ───────────────────────────
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

            if not ctx.remount:
                info(f"Formatting {dev} with ext4...")
                r = run(["mkfs.ext4", "-F", "-q", dev], check=False, timeout=60)
                if r.returncode != 0:
                    ctx.record(f"{label}: mkfs.ext4", False, r.stderr.strip())
                    continue
                ctx.record(f"{label}: mkfs.ext4", True)

            os.makedirs(mount_points[i], exist_ok=True)
            r = run(["mount", dev, mount_points[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: mount", False, r.stderr.strip())
                continue
            mounted[i] = True
            ctx.record(f"{label}: mount", True)

        if not ctx.remount:
            for i in range(N):
                if not mounted[i]:
                    continue
                test_data = f"nasty-iscsi-test{i+1}-{ctx.tag}"
                with open(os.path.join(mount_points[i], "testfile.txt"), "w") as f:
                    f.write(test_data)
                ctx.record(f"iSCSI[{i+1}]: write", True)

        for i in range(N):
            if not mounted[i]:
                continue
            expected = f"nasty-iscsi-test{i+1}-{ctx.tag}"
            with open(os.path.join(mount_points[i], "testfile.txt")) as f:
                got = f.read()
            ctx.record(f"iSCSI[{i+1}]: read/verify", got == expected,
                       "" if got == expected else f"expected '{expected}', got '{got}'")

        if ctx.remount:
            return

        # ── Flush before snapshotting ─────────────────────────────
        for i in range(N):
            if mounted[i]:
                run(["sync", "-f", mount_points[i]], check=False)

        # ── Snapshots ─────────────────────────────────────────────
        snap_names = [[f"snap-iscsi{i+1}-s{j+1}-{ctx.tag}" for j in range(S)] for i in range(N)]
        for i in range(N):
            for j in range(S):
                label = f"iSCSI[{i+1}]"
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
                ctx.record(f"iSCSI[{i+1}]: snapshot {j+1} listed", found,
                           "" if found else f"'{snap_names[i][j]}' not found")

        # ── Clone ─────────────────────────────────────────────────
        for i in range(N):
            label = f"iSCSI[{i+1}] clone"
            info(f"Cloning '{snap_names[i][0]}' → '{clone_sv_names[i]}'...")
            try:
                clone_sv = await ctx.client.call("snapshot.clone", {
                    "pool": ctx.pool,
                    "subvolume": sv_names[i],
                    "snapshot": snap_names[i][0],
                    "new_name": clone_sv_names[i],
                })
                ctx.record(f"{label}: subvolume created", True)
            except Exception as e:
                ctx.record(f"{label}: subvolume created", False, str(e))
                continue

            try:
                attached = await ctx.client.call("subvolume.attach", {
                    "pool": ctx.pool, "name": clone_sv_names[i],
                })
                block_dev = attached.get("block_device")
                if not block_dev:
                    ctx.record(f"{label}: attach", False, "no block_device")
                    continue
                ctx.record(f"{label}: attach", True)
            except Exception as e:
                ctx.record(f"{label}: attach", False, str(e))
                continue

            try:
                target = await ctx.client.call("share.iscsi.create_quick", {
                    "name": f"test-iscsi{i+1}-clone-{ctx.tag}",
                    "device_path": block_dev,
                })
                clone_target_ids[i] = target["id"]
                ctx.record(f"{label}: target created", True)
            except Exception as e:
                ctx.record(f"{label}: target created", False, str(e))

        await asyncio.sleep(3)

        for i in range(N):
            if clone_target_ids[i] is None:
                continue
            label = f"iSCSI[{i+1}] clone"
            r = run(["iscsiadm", "-m", "discovery", "-t", "sendtargets", "-p", ctx.host], check=False)
            if clone_iqns[i] not in r.stdout:
                ctx.record(f"{label}: read/verify", False, "IQN not discovered")
                continue
            r = run(["iscsiadm", "-m", "node", "-T", clone_iqns[i], "-p", f"{ctx.host}:3260", "--login"],
                    check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: read/verify", False, f"login: {r.stderr.strip()}")
                continue
            clone_connected[i] = True

            await asyncio.sleep(2)
            dev = find_iscsi_device(clone_iqns[i])
            if not dev:
                ctx.record(f"{label}: read/verify", False, "device not found")
                continue

            os.makedirs(clone_mounts[i], exist_ok=True)
            r = run(["mount", dev, clone_mounts[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: read/verify", False, f"mount: {r.stderr.strip()}")
                continue
            clone_mounted[i] = True

            expected = f"nasty-iscsi-test{i+1}-{ctx.tag}"
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
                        ctx.record(f"iSCSI[{i+1}]: snapshot {j+1} deleted", True)
                    except Exception as e:
                        ctx.record(f"iSCSI[{i+1}]: snapshot {j+1} deleted", False, str(e))

    except Exception as e:
        ctx.record("iSCSI: test", False, str(e))
    finally:
        for i in range(N):
            if clone_mounted[i]:
                run(["umount", clone_mounts[i]], check=False)
            if os.path.isdir(clone_mounts[i]):
                os.rmdir(clone_mounts[i])
            if clone_connected[i]:
                run(["iscsiadm", "-m", "node", "-T", clone_iqns[i], "-p", f"{ctx.host}:3260", "--logout"],
                    check=False)
            run(["iscsiadm", "-m", "node", "-T", clone_iqns[i], "-p", f"{ctx.host}:3260", "-o", "delete"],
                check=False)
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if logged_in[i]:
                run(["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "--logout"],
                    check=False)
            run(["iscsiadm", "-m", "node", "-T", iqns[i], "-p", f"{ctx.host}:3260", "-o", "delete"],
                check=False)
            if not ctx.skip_delete:
                if clone_target_ids[i]:
                    try:
                        await ctx.client.call("share.iscsi.delete", {"id": clone_target_ids[i]})
                    except Exception:
                        pass
                try:
                    await ctx.client.call("subvolume.detach", {"pool": ctx.pool, "name": clone_sv_names[i]})
                except Exception:
                    pass
                try:
                    await ctx.client.call("subvolume.delete", {"pool": ctx.pool, "name": clone_sv_names[i]})
                except Exception:
                    pass
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
