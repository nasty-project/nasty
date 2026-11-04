"""Run only inside the disposable appliance-smoke VM and its blank test pool."""
import hashlib
import itertools
import json
import os
import stat
from pathlib import Path
import subprocess


def atomic_provisioning_smoke(ws, call, recv_response, pool, mount_point):
    ids = itertools.count(7000)

    def rpc(method, params):
        return call(ws, method, next(ids), params)

    def rejected(method, params):
        request_id = next(ids)
        ws.send(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}))
        response = recv_response(ws, request_id)
        assert response.get("error"), response

    def properties(name):
        return {"nasty-csi:managed_by": "nasty-csi", "nasty-csi:volume_id": name,
                "nasty-csi:delete_strategy": "retain"}

    # Permission changes are confined to this VM's blank, disposable test pool.
    # The API must refuse an unsafe parent and must not harden it automatically.
    assert pool == "smoke-pool" and mount_point == "/fs/smoke-pool"
    os.chmod(mount_point, 0o777)
    rejected("subvolume.create_atomic", {"filesystem": pool, "name": "atomic-unsafe-parent",
                                        "subvolume_type": "filesystem", "initial_properties": properties("atomic-unsafe-parent")})
    assert stat.S_IMODE(os.stat(mount_point).st_mode) == 0o777
    assert not (Path(mount_point) / "atomic-unsafe-parent").exists()
    os.chmod(mount_point, 0o755)

    source_props = {**properties("atomic-fs"), "nasty-csi:share_id": "source-only"}
    params = {"filesystem": pool, "name": "atomic-fs", "subvolume_type": "filesystem",
              "volsize_bytes": 67108864, "initial_properties": source_props}
    source = rpc("subvolume.create_atomic", params)
    assert source["created"] and source["properties"] == source_props, source
    assert not rpc("subvolume.create_atomic", params).get("created", False)
    rejected("subvolume.create_atomic", {**params, "initial_properties": properties("foreign")})
    with open(Path(source["path"]) / "must-survive", "wb") as data:
        data.write(b"atomic-clone-fixture")
        data.flush()
        os.fsync(data.fileno())

    clone_params = {"filesystem": pool, "name": "atomic-fs", "new_name": "atomic-clone",
                    "initial_properties": properties("atomic-clone")}
    clone = rpc("subvolume.clone_atomic", clone_params)
    assert clone["created"] and clone["properties"] == clone_params["initial_properties"], clone
    assert (Path(clone["path"]) / "must-survive").read_bytes() == b"atomic-clone-fixture"
    assert not rpc("subvolume.clone_atomic", clone_params).get("created", False)
    rpc("snapshot.create", {"filesystem": pool, "subvolume": "atomic-fs", "name": "atomic-point", "read_only": True})
    snap_clone = rpc("snapshot.clone_atomic", {
        "filesystem": pool, "subvolume": "atomic-fs", "snapshot": "atomic-point",
        "new_name": "atomic-snapshot-clone", "initial_properties": properties("atomic-snapshot-clone"),
    })
    assert snap_clone["created"] and snap_clone["properties"] == properties("atomic-snapshot-clone"), snap_clone
    assert (Path(snap_clone["path"]) / "must-survive").read_bytes() == b"atomic-clone-fixture"

    block = rpc("subvolume.create_atomic", {
        "filesystem": pool, "name": "atomic-block", "subvolume_type": "block",
        "volsize_bytes": 67108864, "block_filesystem": "ext4",
        "initial_properties": properties("atomic-block"),
    })
    assert block["created"] and block["block_device"] and block["block_filesystem_uuid"], block
    backing = subprocess.check_output(["losetup", "--noheadings", "--output", "BACK-FILE", block["block_device"]], text=True).strip()
    assert backing == str(Path(block["path"]) / "vol.img"), (backing, block)
    block_clone = rpc("subvolume.clone_atomic", {
        "filesystem": pool, "name": "atomic-block", "new_name": "atomic-block-clone",
        "initial_properties": properties("atomic-block-clone"),
    })
    assert block_clone["created"] and block_clone["block_device"], block_clone
    assert block_clone["block_filesystem_uuid"] == block["block_filesystem_uuid"], block_clone
    assert block_clone["properties"] == properties("atomic-block-clone"), block_clone

    # Simulate interruption after durable reservation, before storage creation.
    # Do not remove the journal: the API must preserve it and refuse both the
    # atomic and legacy mutation paths rather than automatically recreating data.
    pending_name = "atomic-interrupted"
    reservation = Path(mount_point) / ".nasty/provisioning" / hashlib.sha256(pending_name.encode()).hexdigest()
    reservation.mkdir(mode=0o700)
    (reservation / "request.json").write_text(json.dumps({"destination": pending_name, "initial_properties": properties(pending_name)}))
    pending = {"filesystem": pool, "name": pending_name, "subvolume_type": "filesystem"}
    rejected("subvolume.create_atomic", {**pending, "initial_properties": properties(pending_name)})
    rejected("subvolume.create", pending)
    assert reservation.exists() and not (Path(mount_point) / pending_name).exists()
    visible = rpc("subvolume.list", {"filesystem": pool})
    assert all(not volume["name"].startswith(".nasty") for volume in visible), visible
