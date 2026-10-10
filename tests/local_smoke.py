#!/usr/bin/env python3
"""Isolated Docker smoke test: local blobs, nested base path, failure and timeout.

Run on a Linux Docker host after building both images. No existing containers,
networks, databases or volumes are changed. Secrets are generated per run and are
never printed. Only the resources created by this invocation are removed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def docker(*args, check=True):
    result = subprocess.run(["docker", *args], text=True, capture_output=True)
    if check and result.returncode:
        raise RuntimeError(f"Docker {args[0]} failed with exit code {result.returncode}")
    return result.stdout.strip()


def env_file(directory, name, values):
    path = directory / name
    path.write_text("".join(f"{key}={value}\n" for key, value in values.items()))
    path.chmod(0o600)
    return str(path)


def wait_for(callback, label, seconds=60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = callback()
            if value:
                return value
        except (OSError, RuntimeError):
            pass
        time.sleep(0.5)
    raise RuntimeError(f"Timed out waiting for {label}")


def request(url, method="GET", data=None, binary=False):
    headers = {"X-Job-Request": "1"}
    if data is not None and not isinstance(data, bytes):
        headers["Content-Type"] = "application/json"
        data = json.dumps(data).encode()
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data, method=method, headers=headers), timeout=15) as response:
            content = response.read()
            return content if binary else json.loads(content or b"null")
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"HTTP {method} {urllib.parse.urlparse(url).path} failed with status {error.code}") from None
    except urllib.error.URLError:
        raise RuntimeError(f"HTTP {method} transport failed") from None


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file_pointer, code, message, headers, new_url):
        return None


def verify_redirect(url):
    try:
        urllib.request.build_opener(NoRedirect()).open(url, timeout=15)
    except urllib.error.HTTPError as error:
        assert error.code in (301, 308), "expected permanent slash redirect"
        assert error.headers["Location"] == "/nested/jobs/", "incorrect redirect destination"
        return
    raise AssertionError("base path without a trailing slash did not redirect")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-image", default="ghcr.io/uiui611/job-manage-server:main")
    parser.add_argument("--worker-image", default="ghcr.io/uiui611/job-manage-worker:main")
    args = parser.parse_args()
    prefix = f"job-manage-local-{uuid.uuid4().hex[:10]}"
    network = prefix + "-net"
    containers = []
    network_created = False
    base_path = "/nested/jobs"
    checks = []

    def passed(name):
        checks.append(name)
        print("PASS " + name, flush=True)

    try:
        with tempfile.TemporaryDirectory(prefix=prefix + "-") as temporary:
            directory = Path(temporary)
            password, api_token, worker_token = (secrets.token_hex(24) for _ in range(3))
            database_env = env_file(directory, "database.env", {
                "POSTGRES_USER": "job_local", "POSTGRES_DB": "job_local", "POSTGRES_PASSWORD": password,
            })
            server_env = env_file(directory, "server.env", {
                "JOB_DATABASE_URL": f"postgres://job_local:{password}@postgres:5432/job_local",
                "JOB_API_TOKEN": api_token, "JOB_WORKER_TOKEN": worker_token,
                "JOB_SIGNING_KEY": secrets.token_hex(32), "JOB_AUTH_MODE": "none",
                "JOB_BASE_PATH": base_path, "JOB_BIND": "0.0.0.0:8080",
                "JOB_STORAGE_DIR": "/data", "JOB_WEB_DIR": "/app/web",
            })
            worker_env = env_file(directory, "worker.env", {
                "JOB_SERVER_URL": "http://server:8080" + base_path,
                "JOB_ALLOW_INSECURE_HTTP": "true", "JOB_WORKER_TOKEN": worker_token,
                "JOB_WORKER_ID": "local-smoke", "JOB_WORKER_NAME": "Isolated local smoke worker",
                "JOB_WORKER_TAGS": '["os=linux","env=local-smoke"]', "JOB_WORK_DIR": "/work",
            })
            docker("network", "create", network)
            network_created = True
            database = prefix + "-postgres"
            containers.append(database)
            docker("run", "-d", "--name", database, "--network", network, "--network-alias", "postgres",
                   "--env-file", database_env, "--tmpfs", "/var/lib/postgresql/data:rw", "postgres:17-bookworm")
            wait_for(lambda: "accepting connections" in docker("exec", database, "pg_isready", "-U", "job_local", "-d", "job_local", check=False), "temporary PostgreSQL")
            server = prefix + "-server"
            containers.append(server)
            docker("run", "-d", "--name", server, "--network", network, "--network-alias", "server",
                   "-p", "127.0.0.1::8080", "--env-file", server_env, "--read-only", "--cap-drop", "ALL",
                   "--security-opt", "no-new-privileges:true", "--tmpfs", "/data:uid=10001,gid=10001,mode=0700",
                   "--tmpfs", "/tmp:uid=10001,gid=10001,mode=1777", args.server_image)
            port = docker("port", server, "8080/tcp").splitlines()[0].rsplit(":", 1)[1]
            origin = "http://127.0.0.1:" + port
            base = origin + base_path
            api = base + "/api/v1"
            wait_for(lambda: request(base + "/config.json"), "temporary server")
            worker = prefix + "-worker"
            containers.append(worker)
            docker("run", "-d", "--name", worker, "--network", network, "--env-file", worker_env,
                   "--init", "--pids-limit", "256", "--memory", "1g", "--cpus", "2", "--read-only",
                   "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                   "--tmpfs", "/work:uid=10001,gid=10001,mode=0700", "--tmpfs", "/tmp:uid=10001,gid=10001,mode=1777", args.worker_image)
            wait_for(lambda: any(w["status"] == "online" for w in request(api + "/workers")), "temporary worker")

            verify_redirect(base)
            index_failure = None
            try:
                html = request(base + "/", binary=True).decode()
            except RuntimeError as error:
                # Keep checking independent API/file behavior, but fail the overall run.
                index_failure = str(error)
                html = request(base + "/index.html", binary=True).decode()
                print("DIAGNOSTIC explicit index.html works while directory index fails", flush=True)
            assets = re.findall(r'(?:src|href)="([^"\s]+\.(?:js|css)(?:\?[^"\s]*)?)"', html)
            assert assets, "built index must reference JS/CSS assets"
            for asset in assets:
                asset_url = urllib.parse.urljoin(base + "/", asset)
                assert urllib.parse.urlparse(asset_url).path.startswith(base_path + "/"), "asset escaped configured base path"
                assert request(asset_url, binary=True), "asset is empty"
            config = request(base + "/config.json")
            assert config["base_path"] == base_path and config["api_base"] == base_path + "/api/v1"
            if index_failure is None:
                passed("nested base path: index, assets, config and trailing-slash redirect")
            else:
                passed("nested base path: explicit index.html, assets, config and trailing-slash redirect")

            payload = bytes(range(256)) * (4 * 1024 * 1024 // 256)
            digest = hashlib.sha256(payload).hexdigest()
            upload = request(api + "/artifacts/uploads", "POST", {"name": "input.bin", "size": len(payload)})
            assert upload["url"].startswith(base_path + "/api/v1/blobs/"), "expected signed local-storage URL"
            request(urllib.parse.urljoin(origin, upload["url"]), "PUT", payload)
            request(api + "/artifacts/" + upload["id"] + "/complete", "POST", {})
            passed("signed local input PUT accepts 4 MiB above API JSON body limit")

            def run_job(name, script, *, files=None, timeout=30):
                definition = request(api + "/definitions", "POST", {
                    "name": name, "driver": "python", "script": script,
                    "input_schema": {"type": "object"}, "required_tags": ["env=local-smoke"],
                    "timeout_seconds": timeout,
                })
                run = request(api + "/runs", "POST", {"definition_id": definition["id"], "input": {}, "files": files or []})
                def terminal():
                    state = request(api + "/runs/" + run["id"])
                    return state if state["status"] in ("succeeded", "failed", "canceled") else None
                return wait_for(terminal, name + " terminal state")

            script = """import hashlib,json,os,shutil
from pathlib import Path
source=Path(os.environ['JOB_INPUT_DIR'])/'input.bin'
destination=Path(os.environ['JOB_ARTIFACT_DIR'])/'copy.bin'
shutil.copyfile(source,destination)
Path(os.environ['JOB_RESULT_FILE']).write_text(json.dumps({'bytes':destination.stat().st_size,'sha256':hashlib.sha256(destination.read_bytes()).hexdigest()}))
"""
            run = run_job("local-4mib-copy", script, files=[{"name": "input.bin", "artifact_id": upload["id"]}])
            assert run["status"] == "succeeded", "copy job failed"
            assert run["result"] == {"bytes": len(payload), "sha256": digest}, "structured result mismatch"
            artifacts = request(api + "/runs/" + run["id"] + "/artifacts")
            assert len(artifacts) == 1 and artifacts[0]["name"] == "copy.bin"
            downloaded = request(urllib.parse.urljoin(origin, artifacts[0]["url"]), binary=True)
            assert downloaded == payload, "downloaded artifact differs from input"
            passed("Python copy: 4 MiB worker input/output, JSON result, byte-exact signed download")

            failed = run_job("local-nonzero", "raise SystemExit(7)\n")
            assert failed["status"] == "failed" and failed["exit_code"] == 7, "nonzero exit was not recorded"
            passed("nonzero Python exit becomes failed with exit_code=7")
            timed_out = run_job("local-timeout", "import time\ntime.sleep(30)\n", timeout=1)
            assert timed_out["status"] == "failed" and "deadline" in (timed_out["error"] or ""), "timeout did not become failed"
            passed("one-second execution deadline becomes failed")

            active_definition = request(api + "/definitions", "POST", {
                "name": "local-restart-active", "driver": "python", "script": "import time\ntime.sleep(60)\n",
                "required_tags": ["env=local-smoke"], "timeout_seconds": 120,
            })
            active_run = request(api + "/runs", "POST", {"definition_id": active_definition["id"], "input": {}})
            wait_for(lambda: request(api + "/runs/" + active_run["id"])["status"] == "running", "running job before restart")
            queued_definition = request(api + "/definitions", "POST", {
                "name": "local-restart-queued", "driver": "python", "script": "print('must remain queued')\n",
                "required_tags": ["env=no-worker-for-this-tag"], "timeout_seconds": 30,
            })
            queued_run = request(api + "/runs", "POST", {"definition_id": queued_definition["id"], "input": {"marker": "keep"}})
            assert queued_run["status"] == "queued"
            # This server is owned by this invocation. PostgreSQL keeps running.
            docker("restart", "--time", "10", server)
            # Docker may allocate a new host port when a dynamic port is republished.
            port = docker("port", server, "8080/tcp").splitlines()[0].rsplit(":", 1)[1]
            origin = "http://127.0.0.1:" + port
            base = origin + base_path
            api = base + "/api/v1"
            wait_for(lambda: request(base + "/config.json"), "server after restart")
            wait_for(lambda: any(w["status"] == "online" for w in request(api + "/workers")), "worker reconnect after restart")
            interrupted = request(api + "/runs/" + active_run["id"])
            assert interrupted["status"] == "failed", "server restart did not fail active job"
            assert any(reason in (interrupted["error"] or "") for reason in ("server_shutdown", "server_restarted")), "unexpected restart failure reason"
            retained = request(api + "/runs/" + queued_run["id"])
            assert retained["status"] == "queued" and retained["input"] == {"marker": "keep"}, "queued run was not preserved"
            for definition in (active_definition, queued_definition):
                assert request(api + "/definitions/" + definition["id"])["spec"]["name"] == definition["spec"]["name"], "definition did not survive restart"
            passed("server restart: active job failed, queued job and definitions preserved, worker reconnected")
            print(json.dumps({
                "checks_passed": len(checks), "payload_bytes": len(payload), "payload_sha256": digest,
                "base_path": base_path,
                "server_image_id": docker("container", "inspect", "--format", "{{.Image}}", server),
                "worker_image_id": docker("container", "inspect", "--format", "{{.Image}}", worker),
            }), flush=True)
            if index_failure is not None:
                raise RuntimeError(index_failure)
    finally:
        # All names come from this invocation, never a Docker listing or wildcard.
        for container in reversed(containers):
            docker("rm", "-f", container, check=False)
        if network_created:
            docker("network", "rm", network, check=False)
        if any(docker("container", "inspect", "--format", "{{.Name}}", container, check=False) for container in containers):
            raise RuntimeError("A temporary test container could not be removed")
        if network_created and docker("network", "inspect", "--format", "{{.Name}}", network, check=False):
            raise RuntimeError("The temporary test network could not be removed")
        print("CLEANUP temporary containers and network removed; no persistent volumes created", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, RuntimeError) as error:
        print("FAIL " + str(error), flush=True)
        raise SystemExit(1)
