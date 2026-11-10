"""Exercise a built native executable without reading existing accounts or data."""
import json
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


binary = pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="nine-router-smoke-") as directory, tempfile.TemporaryFile() as log:
    process = subprocess.Popen(
        [str(binary), "--headless", "--data-dir", directory, "--listen", "127.0.0.1:0"],
        stdout=log, stderr=log,
    )
    try:
        endpoint = None
        for _ in range(200):
            assert process.poll() is None, "Native executable exited during startup"
            log.seek(0)
            found = re.search(rb"http://127\.0\.0\.1:\d+", log.read())
            if found:
                endpoint = found[0].decode()
                break
            time.sleep(0.05)
        assert endpoint, "Native server did not start within 10 seconds"
        token = pathlib.Path(directory, "admin-token").read_text().strip()
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def request(path, method="GET", body=None, authenticated=True):
            headers = {"Authorization": "Bearer " + token} if authenticated else {}
            if body is not None:
                headers["Content-Type"] = "application/json"
            req = urllib.request.Request(endpoint + path, method=method, headers=headers,
                                         data=json.dumps(body).encode() if body is not None else None)
            try:
                with opener.open(req, timeout=5) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                return error.code, json.load(error)

        assert request("/health", authenticated=False) == (200, {"status": "ok"})
        assert request("/api/status", authenticated=False)[0] == 401
        status, value = request("/api/status")
        assert status == 200 and value["engine"] == "rust" and value["featureComplete"] is False
        status, value = request("/api/providers")
        assert status == 200 and len(value["providers"]) == 143 and value["connections"] == []
        status, value = request("/api/keys", "POST", {"name": "Isolated smoke client"})
        assert status == 201 and value["apiKey"]["key"].startswith("sk-")
        status, listed = request("/api/keys")
        assert status == 200 and "key" not in listed["apiKeys"][0]
        assert request("/api/models/alias", "PUT", {"demo": "openai/gpt-4o"})[0] == 200
        assert request("/api/models/alias")[1]["aliases"] == {"demo": "openai/gpt-4o"}
        process.send_signal(signal.SIGINT)
        assert process.wait(timeout=10) == 0, "Native server did not shut down cleanly"
        print("Native smoke passed: isolated startup, auth, catalog, key creation, aliases and graceful shutdown.")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
