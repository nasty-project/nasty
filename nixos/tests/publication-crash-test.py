"""Deterministic engine-process loss on the disposable appliance-smoke pool.

Test-only command shims and LD_PRELOAD interposition stop operations; the
production engine/API has no fault-injection switches or recovery backdoors.
This test does not claim sudden VM/power-loss durability.
"""
import concurrent.futures
import hashlib
import itertools
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request

import websocket

POOL = "smoke-pool"
ROOT = Path("/fs/smoke-pool")
ARM = Path("/run/nasty-publication-fault")
HIT = Path("/run/nasty-publication-fault-hit")
IDS = itertools.count(8000)


def connect():
    request = urllib.request.Request("http://127.0.0.1:2137/api/login", data=json.dumps({
        "username": "admin", "password": "password-changed-by-smoke-test",
    }).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        token = json.load(response)["token"]
    ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=120)
    ws.send(json.dumps({"token": token}))
    assert json.loads(ws.recv()).get("authenticated"), "crash test authentication failed"
    return ws


def rpc(method, params):
    ws = connect()
    try:
        request_id = next(IDS)
        ws.send(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}))
        while True:
            response = json.loads(ws.recv())
            if response.get("id") == request_id:
                return response
    finally:
        ws.close()


def result(method, params):
    response = rpc(method, params)
    assert "error" not in response, response
    return response["result"]


def props(name):
    return {"nasty-csi:managed_by": "nasty-csi", "nasty-csi:volume_id": name,
            "nasty-csi:delete_strategy": "retain"}


def restart_engine():
    ARM.unlink(missing_ok=True)
    subprocess.run(["systemctl", "kill", "--kill-whom=all", "--signal=SIGKILL", "nasty-engine.service"], check=True)
    subprocess.run(["systemctl", "restart", "nasty-engine.service"], check=True, timeout=60)
    for _ in range(60):
        try:
            result("subvolume.list", {"filesystem": POOL})
            return
        except (OSError, AssertionError, ValueError, websocket.WebSocketException):
            time.sleep(1)
    raise AssertionError("engine did not restore the pool after process loss")


def interrupted(method, params, phase, published):
    name = params.get("new_name", params.get("name"))
    key = hashlib.sha256(name.encode()).hexdigest()
    reservation = ROOT / ".nasty/provisioning" / key
    HIT.unlink(missing_ok=True)
    ARM.write_text(f"{phase}\n{key}\n")
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        future = executor.submit(rpc, method, params)
        try:
            for _ in range(90):
                if HIT.exists():
                    marker = HIT.read_text().split()
                    if len(marker) >= 3 and marker[-1].isdigit():
                        status = Path(f"/proc/{marker[-1]}/status")
                        if status.exists() and "State:\tT" in status.read_text():
                            break
                if future.done():
                    raise AssertionError(f"operation finished without fault interception: {future.result()}")
                time.sleep(0.2)
            else:
                raise AssertionError(f"fault boundary never reached: {method}/{phase}")
            marker = HIT.read_text().split()
            assert marker[0] == phase, marker
            pid = int(marker[-1])
            assert "State:\tT" in Path(f"/proc/{pid}/status").read_text(), marker
            assert (ROOT / name).exists() == published, (phase, name)
            assert reservation.exists() and (reservation / "request.json").exists()
            journal = json.loads((reservation / "request.json").read_text())
            assert journal["initial_properties"] == params["initial_properties"], journal
        finally:
            restart_engine()  # Also kills the paused command shim and all other service children.
        try:
            reply = future.result(timeout=10)
        except (OSError, websocket.WebSocketException):
            reply = None
        assert reply is None or "error" in reply, reply

    if published:
        volume = result("subvolume.get", {"filesystem": POOL, "name": name})
        assert volume["properties"] == params["initial_properties"], volume
        replay = result(method, params)
        assert not replay.get("created", False) and replay["properties"] == volume["properties"], replay
    else:
        assert not (ROOT / name).exists()
        assert "error" in rpc(method, params), "interrupted reservation was automatically replayed"
        assert "error" in rpc("subvolume.create", {"filesystem": POOL, "name": name}), "legacy create bypassed reservation"
        assert reservation.exists() and (reservation / "request.json").exists()
    visible = result("subvolume.list", {"filesystem": POOL})
    assert all(not volume["name"].startswith(".nasty") for volume in visible), visible


def main():
    assert ROOT.is_dir() and os.geteuid() == 0
    os.chmod(ROOT, 0o755)  # This VM's disposable test pool only, never a live share.
    source = result("subvolume.get", {"filesystem": POOL, "name": "atomic-fs"})
    source_properties = source["properties"].copy()
    source_data = (Path(source["path"]) / "must-survive").read_bytes()
    cases = [
        ("subvolume.create_atomic", {"subvolume_type": "block", "volsize_bytes": 67108864,
                                     "block_filesystem": "ext4"}, "block-init", False),
        ("subvolume.clone_atomic", {"name": "atomic-fs"}, "clone-copy", False),
        ("snapshot.clone_atomic", {"subvolume": "atomic-fs", "snapshot": "atomic-point"}, "clone-copy", False),
    ]
    for method, extra, phase, published in cases:
        name = f"atomic-crash-{method.split('.')[0]}-{phase}"
        destination_key = "name" if method == "subvolume.create_atomic" else "new_name"
        params = {"filesystem": POOL, **extra, destination_key: name, "initial_properties": props(name)}
        interrupted(method, params, phase, published)
    for method, extra in [
        ("subvolume.create_atomic", {"subvolume_type": "filesystem"}),
        ("subvolume.clone_atomic", {"name": "atomic-fs"}),
        ("snapshot.clone_atomic", {"subvolume": "atomic-fs", "snapshot": "atomic-point"}),
    ]:
        for phase, published in [("before-rename", False), ("after-rename", True)]:
            name = f"atomic-crash-{method.split('.')[0]}-{method.split('.')[1]}-{phase}"
            destination_key = "name" if method == "subvolume.create_atomic" else "new_name"
            params = {"filesystem": POOL, **extra, destination_key: name, "initial_properties": props(name)}
            interrupted(method, params, phase, published)
    unchanged = result("subvolume.get", {"filesystem": POOL, "name": "atomic-fs"})
    assert unchanged["properties"] == source_properties, unchanged
    assert (Path(source["path"]) / "must-survive").read_bytes() == source_data
    print("PASS: nine real operation interruption boundaries; source and retain policies preserved")


if __name__ == "__main__":
    main()
