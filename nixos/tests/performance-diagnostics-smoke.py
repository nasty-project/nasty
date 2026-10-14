"""Disposable VM only: verify opt-in OS-disk diagnostics across engine boots."""
import json
from pathlib import Path
import sys
import time
import urllib.request
import websocket

request = urllib.request.Request(
    "http://127.0.0.1:2137/api/login",
    data=json.dumps({"username": "admin", "password": "password-changed-by-smoke-test"}).encode(),
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(request, timeout=10) as response:
    token = json.load(response)["token"]
ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=20)
ws.send(json.dumps({"token": token}))
assert json.loads(ws.recv()).get("authenticated")
counter = 0

def rpc(method, params=None):
    global counter
    counter += 1
    ws.send(json.dumps({"jsonrpc": "2.0", "id": counter, "method": method, "params": params}))
    while True:
        response = json.loads(ws.recv())
        if response.get("id") == counter:
            assert "error" not in response, response
            return response["result"]

checkpoint = Path("/var/lib/nasty/performance-diagnostics.json")
try:
    if sys.argv[1] == "enable":
        rpc("system.diagnostics.clear")
        assert not checkpoint.exists()
        rpc("system.diagnostics.capture_reboot", {"enabled": True})
        rpc("fs.list")
        for _ in range(30):
            data = json.loads(checkpoint.read_text())
            if data["sessions"] and "received_to_backend" in {t["stage"] for t in data["sessions"][-1]["timings"]}:
                break
            time.sleep(1)
        else:
            raise AssertionError("no diagnostic checkpoint was written")
        assert checkpoint.stat().st_mode & 0o777 == 0o600
        assert checkpoint.stat().st_size <= 2 * 1024 * 1024
        assert data["sessions"][0]["boot_phases"]
        assert "received_to_backend" in {t["stage"] for t in data["sessions"][0]["timings"]}
    else:
        report = rpc("system.diagnostics.report")
        capture = report["persistent_capture"]
        assert capture["active"] and capture["previous_sessions"], capture
        assert all(session["epoch"] != report["epoch"] for session in capture["previous_sessions"])
        assert any(session["boot_phases"] for session in capture["previous_sessions"])
        if sys.argv[1] == "verify":
            for _ in range(30):
                data = json.loads(checkpoint.read_text())
                if data["sessions"][-1]["epoch"] == report["epoch"]:
                    break
                time.sleep(1)
            else:
                raise AssertionError("capture did not resume after engine restart")
        if sys.argv[1] == "clear":
            assert len(capture["previous_sessions"]) == 2, capture
            rpc("system.diagnostics.clear")
            assert not checkpoint.exists()
            report = rpc("system.diagnostics.report")
            assert not report["persistent_capture"]["active"]
            assert report["persistent_capture"]["previous_sessions"] == []
            assert not report["detailed_capture"]
finally:
    ws.close()
