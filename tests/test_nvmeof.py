import asyncio
import glob
import json
import os

from nasty.context import TestContext
from nasty.output import info
from nasty.shell import run

N = 5  # subvolumes per run
S = 2  # snapshots per subvolume


def find_nvme_device(nqn: str) -> str | None:
    """Return /dev/nvmeXnY for a connected NVMe-oF subsystem by NQN."""
    r = run(["nvme", "list-subsys", "-o", "json"], check=False)
    if r.returncode != 0:
        return None
    try:
        data = json.loads(r.stdout)
        subsystems = data if isinstance(data, list) else data.get("Subsystems", [])
        for subsys in subsystems:
            if subsys.get("NQN") == nqn or subsys.get("SubsystemNQN") == nqn:
                for path in subsys.get("Paths", []):
                    name = path.get("Name", "")
                    if name:
                        return f"/dev/{name}n1"
                for ns in subsys.get("Namespaces", []):
                    name = ns.get("NameSpace") or ns.get("Name", "")
                    if name:
                        return f"/dev/{name}"
    except (json.JSONDecodeError, KeyError):
        pass
    # Fallback: scan /dev and check subnqn via nvme id-ctrl
    for dev in sorted(glob.glob("/dev/nvme*n1")):
        r2 = run(["nvme", "id-ctrl", dev, "-o", "json"], check=False)
        if r2.returncode == 0:
            try:
                if json.loads(r2.stdout).get("subnqn") == nqn:
                    return dev
            except (json.JSONDecodeError, KeyError):
                pass
    return None


async def test_nvmeof(ctx: TestContext):
    """Create N NVMe-oF shares, connect all, format+write/read, take S snapshots each, cleanup."""
    from nasty.output import header
    header(f"NVMe-oF Tests (x{N} concurrent)")

    sv_names     = [f"test-nvme{i}-{ctx.tag}"         for i in range(1, N + 1)]
    subsys_names = [f"test-nvme{i}-{ctx.tag}"         for i in range(1, N + 1)]
    nqns         = [f"nqn.2024-01.com.nasty:{n}"       for n in subsys_names]
    mount_points = [f"/tmp/nasty-test-nvme{i}-{ctx.tag}" for i in range(1, N + 1)]
    subsys_ids      = [None] * N
    connected       = [False] * N
    mounted         = [False] * N
    clone_sv_names  = [f"test-nvme{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_nqns      = [f"nqn.2024-01.com.nasty:test-nvme{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_subsys_ids = [None] * N
    clone_mounts    = [f"/tmp/nasty-test-nvme{i+1}-clone-{ctx.tag}" for i in range(N)]
    clone_connected = [False] * N
    clone_mounted   = [False] * N

    try:
        # ── Create block subvolumes + shares ──────────────────────
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

        # ── Connect ───────────────────────────────────────────────
        for i in range(N):
            label = f"NVMe-oF[{i+1}]"
            info(f"Connecting to NVMe-oF target {nqns[i]}...")
            r = run(["nvme", "connect", "-t", "tcp", "-n", nqns[i], "-a", ctx.host, "-s", "4420"],
                    check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: connect", False, r.stderr.strip())
                continue
            connected[i] = True
            ctx.record(f"{label}: connect", True)

        await asyncio.sleep(3)

        # ── Format + mount + write/read ───────────────────────────
        for i in range(N):
            if not connected[i]:
                continue
            label = f"NVMe-oF[{i+1}]"

            dev = find_nvme_device(nqns[i])
            if not dev:
                ctx.record(f"{label}: device visible", False, "NQN not found in nvme list-subsys")
                continue
            ctx.record(f"{label}: device visible", True)

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

        for i in range(N):
            if not mounted[i]:
                continue
            test_data = f"nasty-nvme-test{i+1}-{ctx.tag}"
            with open(os.path.join(mount_points[i], "testfile.txt"), "w") as f:
                f.write(test_data)
            ctx.record(f"NVMe-oF[{i+1}]: write", True)

        for i in range(N):
            if not mounted[i]:
                continue
            expected = f"nasty-nvme-test{i+1}-{ctx.tag}"
            with open(os.path.join(mount_points[i], "testfile.txt")) as f:
                got = f.read()
            ctx.record(f"NVMe-oF[{i+1}]: read/verify", got == expected,
                       "" if got == expected else f"expected '{expected}', got '{got}'")

        # ── Snapshots ─────────────────────────────────────────────
        # Flush initiator page cache to the NVMe-oF targets before snapshotting
        run(["sync"], check=False)

        snap_names = [[f"snap-nvme{i+1}-s{j+1}-{ctx.tag}" for j in range(S)] for i in range(N)]
        for i in range(N):
            for j in range(S):
                label = f"NVMe-oF[{i+1}]"
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
                ctx.record(f"NVMe-oF[{i+1}]: snapshot {j+1} listed", found,
                           "" if found else f"'{snap_names[i][j]}' not found")

        # ── Clone ─────────────────────────────────────────────────
        for i in range(N):
            label = f"NVMe-oF[{i+1}] clone"
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
                subsys = await ctx.client.call("share.nvmeof.create_quick", {
                    "name": f"test-nvme{i+1}-clone-{ctx.tag}",
                    "device_path": block_dev,
                })
                clone_subsys_ids[i] = subsys["id"]
                ctx.record(f"{label}: share created", True)
            except Exception as e:
                ctx.record(f"{label}: share created", False, str(e))

        await asyncio.sleep(3)

        for i in range(N):
            if clone_subsys_ids[i] is None:
                continue
            label = f"NVMe-oF[{i+1}] clone"
            r = run(["nvme", "connect", "-t", "tcp", "-n", clone_nqns[i], "-a", ctx.host, "-s", "4420"],
                    check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: read/verify", False, f"connect: {r.stderr.strip()}")
                continue
            clone_connected[i] = True

            await asyncio.sleep(2)
            dev = find_nvme_device(clone_nqns[i])
            if not dev:
                ctx.record(f"{label}: read/verify", False, "device not found")
                continue

            os.makedirs(clone_mounts[i], exist_ok=True)
            r = run(["mount", dev, clone_mounts[i]], check=False)
            if r.returncode != 0:
                ctx.record(f"{label}: read/verify", False, f"mount: {r.stderr.strip()}")
                continue
            clone_mounted[i] = True

            expected = f"nasty-nvme-test{i+1}-{ctx.tag}"
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
                        ctx.record(f"NVMe-oF[{i+1}]: snapshot {j+1} deleted", True)
                    except Exception as e:
                        ctx.record(f"NVMe-oF[{i+1}]: snapshot {j+1} deleted", False, str(e))

    except Exception as e:
        ctx.record("NVMe-oF: test", False, str(e))
    finally:
        for i in range(N):
            if clone_mounted[i]:
                run(["umount", clone_mounts[i]], check=False)
            if os.path.isdir(clone_mounts[i]):
                os.rmdir(clone_mounts[i])
            if clone_connected[i]:
                run(["nvme", "disconnect", "-n", clone_nqns[i]], check=False)
            if mounted[i]:
                run(["umount", mount_points[i]], check=False)
            if os.path.isdir(mount_points[i]):
                os.rmdir(mount_points[i])
            if connected[i]:
                run(["nvme", "disconnect", "-n", nqns[i]], check=False)
            if not ctx.skip_delete:
                if clone_subsys_ids[i]:
                    try:
                        await ctx.client.call("share.nvmeof.delete", {"id": clone_subsys_ids[i]})
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
