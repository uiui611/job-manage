#!/usr/bin/env python3
"""API / real runtime / connection-loss smoke tests using Python standard library.

JOB_TEST_URL=http://127.0.0.1:8080/jobs
JOB_TEST_API_TOKEN=... JOB_TEST_WORKER_TOKEN=... python3 tests/smoke.py

Creates definitions, runs, and a temporary token in a dedicated development instance.
Requires a real worker with env=base for Node/Python tests. Never prints credentials.
"""
import base64
import hashlib
import json
import os
import socket
import ssl
import struct
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


BASE = os.environ.get("JOB_TEST_URL", "http://127.0.0.1:8080/jobs").rstrip("/")
TOKEN = os.environ["JOB_TEST_API_TOKEN"]
WORKER_TOKEN = os.environ["JOB_TEST_WORKER_TOKEN"]
PREFIX = "smoke-" + uuid.uuid4().hex[:8]
TERMINAL = {"succeeded", "failed", "canceled"}


def request(method, path, data=None, auth=True, headers=None, expected=(200, 201, 202, 204)):
    url = path if path.startswith(("http://", "https://")) else BASE + "/api/v1" + path
    request_headers = {"X-Job-Request": "1"}
    if auth:
        request_headers["Authorization"] = "Bearer " + TOKEN
    if data is not None:
        data = json.dumps(data).encode()
        request_headers["Content-Type"] = "application/json"
    request_headers.update(headers or {})
    req = urllib.request.Request(url, data=data, headers=request_headers, method=method)
    try:
        response = urllib.request.urlopen(req, timeout=15)
    except urllib.error.HTTPError as error:
        response = error
    body = response.read()
    assert response.status in expected, (method, path, response.status, body[:600].decode(errors="replace"))
    return json.loads(body) if body else None


def fetch_url(url, method="GET", data=None):
    url = urllib.parse.urljoin(BASE + "/", url)
    req = urllib.request.Request(url, data=data, method=method)
    with urllib.request.urlopen(req, timeout=30) as response:
        return response.read()


def definition(driver="node", script=None, tags=None, schema=None):
    return request("POST", "/definitions", {
        "name": PREFIX + "-" + uuid.uuid4().hex[:5], "description": "Integration smoke test",
        "driver": driver, "script": script or NODE_SUM, "required_tags": tags if tags is not None else ["env=base"],
        "input_schema": schema or {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"], "additionalProperties": False},
        "timeout_seconds": 30,
    })


def submit(definition_id, value=None, **kwargs):
    data = {"definition_id": definition_id, "input": value if value is not None else {"a": 2, "b": 3}}
    data.update(kwargs)
    return request("POST", "/runs", data)


def wait_run(run_id, statuses=TERMINAL, timeout=75):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = request("GET", "/runs/" + run_id)
        if value["status"] in statuses:
            return value
        time.sleep(0.25)
    raise AssertionError("run timeout " + run_id + ": " + str(value))


NODE_SUM = """import fs from 'node:fs';
const input = JSON.parse(fs.readFileSync(process.env.JOB_INPUT_FILE, 'utf8'));
console.log('node smoke');
fs.writeFileSync(process.env.JOB_RESULT_FILE, JSON.stringify({sum: input.a + input.b}));
"""
PYTHON_SUM = """import json, os
with open(os.environ['JOB_INPUT_FILE']) as f: data = json.load(f)
print('python smoke', flush=True)
with open(os.environ['JOB_RESULT_FILE'], 'w') as f: json.dump({'sum': data['a'] + data['b']}, f)
"""


class WorkerSocket:
    """Small RFC 6455 client, only for exercising the public worker protocol."""
    def __init__(self, tag, version="0.1.0"):
        parsed = urllib.parse.urlsplit(BASE)
        self.sock = socket.create_connection((parsed.hostname, parsed.port or (443 if parsed.scheme == "https" else 80)), timeout=15)
        if parsed.scheme == "https":
            self.sock = ssl.create_default_context().wrap_socket(self.sock, server_hostname=parsed.hostname)
        key = base64.b64encode(os.urandom(16)).decode()
        path = parsed.path.rstrip("/") + "/worker"
        headers = ["GET " + path + " HTTP/1.1", "Host: " + parsed.netloc, "Upgrade: websocket", "Connection: Upgrade", "Sec-WebSocket-Key: " + key, "Sec-WebSocket-Version: 13", "Authorization: Bearer " + WORKER_TOKEN, "", ""]
        self.sock.sendall("\r\n".join(headers).encode())
        response = bytearray()
        while not response.endswith(b"\r\n\r\n"):
            response.extend(self._read(1))
            assert len(response) < 16384, "oversized handshake"
        assert response.startswith(b"HTTP/1.1 101"), response.decode(errors="replace").splitlines()[0]
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        assert accept.lower() in response.lower(), "invalid WebSocket accept"
        self.send({"type": "hello", "worker_id": PREFIX + "-" + tag, "name": "Smoke protocol worker", "version": version, "protocol_version": 1, "tags": [tag], "drivers": ["node", "python"]})

    def _read(self, count):
        buf = bytearray()
        while len(buf) < count:
            part = self.sock.recv(count - len(buf))
            if not part:
                raise EOFError("WebSocket closed")
            buf.extend(part)
        return bytes(buf)

    def frame(self, opcode, data):
        mask = os.urandom(4)
        header = bytes([0x80 | opcode])
        if len(data) < 126:
            header += bytes([0x80 | len(data)])
        elif len(data) < 65536:
            header += bytes([0xFE]) + struct.pack("!H", len(data))
        else:
            header += bytes([0xFF]) + struct.pack("!Q", len(data))
        self.sock.sendall(header + mask + bytes(value ^ mask[i % 4] for i, value in enumerate(data)))

    def send(self, value):
        self.frame(1, json.dumps(value).encode())

    def receive(self):
        while True:
            first, second = self._read(2)
            size = second & 127
            if size == 126:
                size = struct.unpack("!H", self._read(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self._read(8))[0]
            assert size <= 8 * 1024 * 1024, "oversized WebSocket frame"
            mask = self._read(4) if second & 128 else None
            payload = self._read(size)
            if mask:
                payload = bytes(value ^ mask[i % 4] for i, value in enumerate(payload))
            opcode = first & 15
            if opcode == 8:
                raise EOFError("WebSocket close frame")
            if opcode == 9:
                self.frame(10, payload)
            elif opcode == 1:
                return json.loads(payload)

    def expect(self, kind):
        for _ in range(20):
            value = self.receive()
            if value["type"] == kind:
                return value
            if value["type"] == "error":
                raise AssertionError(value)
            self.send({"type": "heartbeat"})
        raise AssertionError("message not received: " + kind)

    def close(self):
        self.sock.close()


def main():
    request("GET", "/definitions", auth=False, expected=(401, 403))
    print("PASS API rejects unauthenticated requests")
    for driver, script in [("node", NODE_SUM), ("python", PYTHON_SUM)]:
        item = definition(driver, script)
        run = submit(item["id"])
        result = wait_run(run["id"])
        assert result["status"] == "succeeded", result
        assert result["result"]["sum"] == 5, result
        logs = request("GET", "/runs/" + run["id"] + "/logs")
        assert driver + " smoke" in logs["text"], logs
        print("PASS " + driver + " runtime, result, logs")

    request("POST", "/runs", {"definition_id": item["id"], "input": {"a": "wrong", "b": 3}}, expected=(400, 422))
    key = PREFIX + "-idempotent"
    payload = {"definition_id": item["id"], "input": {"a": 1, "b": 4}}
    first = request("POST", "/runs", payload, headers={"Idempotency-Key": key})
    second = request("POST", "/runs", payload, headers={"Idempotency-Key": key})
    assert first["id"] == second["id"], (first, second)
    request("POST", "/runs", {**payload, "input": {"a": 9, "b": 9}}, headers={"Idempotency-Key": key}, expected=(409,))
    assert wait_run(first["id"])["status"] == "succeeded"
    retry = request("POST", "/runs/" + first["id"] + "/retry", {})
    assert retry["id"] != first["id"]
    assert wait_run(retry["id"])["status"] == "succeeded"
    print("PASS input schema, idempotency, explicit manual retry")

    blocked_def = definition(tags=[PREFIX + "-unavailable"])
    blocked = submit(blocked_def["id"])
    following = submit(item["id"])
    assert wait_run(following["id"])["status"] == "succeeded"
    assert request("GET", "/runs/" + blocked["id"])["status"] == "queued"
    request("POST", "/runs/" + blocked["id"] + "/cancel", {})
    assert wait_run(blocked["id"])["status"] == "canceled"
    print("PASS unmatched queued job does not block later jobs")

    slow = definition("python", "import time\nprint('started',flush=True)\ntime.sleep(300)\n")
    running = submit(slow["id"])
    wait_run(running["id"], {"running"})
    request("POST", "/runs/" + running["id"] + "/cancel", {})
    assert wait_run(running["id"])["status"] == "canceled"
    print("PASS cancellation of running process")

    content = b"job-platform-file-transfer\n"
    upload = request("POST", "/artifacts/uploads", {"name": "input.txt", "size": len(content)})
    fetch_url(upload["url"], "PUT", content)
    request("POST", "/artifacts/" + upload["id"] + "/complete", {})
    file_script = """import os, json, pathlib
data = (pathlib.Path(os.environ['JOB_INPUT_DIR']) / 'input.txt').read_bytes()
(pathlib.Path(os.environ['JOB_ARTIFACT_DIR']) / 'output.txt').write_bytes(data.upper())
pathlib.Path(os.environ['JOB_RESULT_FILE']).write_text(json.dumps({'bytes':len(data)}))
"""
    file_def = definition("python", file_script)
    file_run = submit(file_def["id"], files=[{"name": "input.txt", "artifact_id": upload["id"]}])
    assert wait_run(file_run["id"])["status"] == "succeeded"
    artifacts = request("GET", "/runs/" + file_run["id"] + "/artifacts")
    output = next(a for a in artifacts if a["name"] == "output.txt")
    assert fetch_url(output["url"]) == content.upper()
    print("PASS separate HTTP input and output file transfer")

    tag = PREFIX + "-revision"
    pinned = definition(tags=[tag])
    queued = submit(pinned["id"])
    updated = {"name": PREFIX + "-revised", "driver": "node", "script": "throw new Error('new revision');", "required_tags": [tag], "input_schema": {"type": "object"}, "timeout_seconds": 30}
    request("PUT", "/definitions/" + pinned["id"], updated)
    fake = WorkerSocket(tag)
    fake.expect("welcome")
    assignment = fake.expect("assign")["assignment"]
    assert assignment["run_id"] == queued["id"]
    assert assignment["definition"]["script"] == NODE_SUM, assignment
    fake.send({"type": "started", "attempt_id": assignment["attempt_id"]})
    fake.send({"type": "finished", "attempt_id": assignment["attempt_id"], "outcome": "succeeded", "exit_code": 0, "result": {"sum": 5}, "error": None, "artifact_ids": []})
    assert fake.expect("finished_ack")["accepted"]
    fake.close()
    assert wait_run(queued["id"])["status"] == "succeeded"
    print("PASS submitted run pins definition revision")

    tag = PREFIX + "-disconnect"
    lost_def = definition(tags=[tag])
    lost_run = submit(lost_def["id"])
    fake = WorkerSocket(tag)
    fake.expect("welcome")
    assignment = fake.expect("assign")["assignment"]
    fake.send({"type": "started", "attempt_id": assignment["attempt_id"]})
    wait_run(lost_run["id"], {"running"})
    fake.close()
    result = wait_run(lost_run["id"], timeout=45)
    assert result["status"] == "failed", result
    fake = WorkerSocket(tag)
    fake.expect("welcome")
    time.sleep(1)
    assert request("GET", "/runs/" + lost_run["id"])["status"] == "failed"
    fake.send({"type": "finished", "attempt_id": assignment["attempt_id"], "outcome": "succeeded", "exit_code": 0, "result": {}, "error": None, "artifact_ids": []})
    value = fake.receive()
    assert value["type"] == "error" or (value["type"] == "finished_ack" and not value["accepted"]), value
    fake.close()
    assert request("GET", "/runs/" + lost_run["id"])["status"] == "failed"
    print("PASS disconnection fails run, no requeue, stale result rejected")

    bad = WorkerSocket(PREFIX + "-badversion", version="0.0.0-invalid")
    try:
        reply = bad.receive()
        assert reply["type"] == "error", reply
    except EOFError:
        pass
    finally:
        bad.close()
    print("PASS version mismatch rejected")
    print("ALL SMOKE TESTS PASSED (prefix " + PREFIX + ")")


if __name__ == "__main__":
    main()
