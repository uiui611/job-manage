#!/usr/bin/env python3
"""Run smoke.py inside the dedicated Kubernetes namespace, without exposing tokens."""
import argparse
import json
import pathlib
import subprocess
import time
import uuid


def kube(*args, data=None):
    result = subprocess.run(["kubectl", *args], input=data, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError("kubectl failed: " + " ".join(args[:4]) + "\n" + result.stderr)
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--namespace", default="job-manage", help="Dedicated test namespace (default: job-manage)")
    parser.add_argument("--secret-name", default="job-manage-secrets", help="Existing application Secret (default: job-manage-secrets)")
    parser.add_argument("--node", help="Optional Kubernetes node name for this smoke test")
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent
    namespace = args.namespace
    document = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "job-manage-tests", "namespace": namespace}, "data": {"smoke.py": (root / "smoke.py").read_text(encoding="utf-8")}}
    kube("apply", "-f", "-", data=json.dumps(document))
    job = json.loads(kube("create", "--dry-run=client", "-f", str(root / "cluster-job.yaml"), "-o", "json"))
    name = "job-manage-smoke-" + uuid.uuid4().hex[:8]
    job["metadata"]["name"] = name
    job["metadata"]["namespace"] = namespace
    for container in job["spec"]["template"]["spec"]["containers"]:
        for variable in container.get("env", []):
            secret = variable.get("valueFrom", {}).get("secretKeyRef")
            if secret:
                secret["name"] = args.secret_name
    if args.node:
        spec = job["spec"]["template"]["spec"]
        spec["nodeSelector"] = {"kubernetes.io/hostname": args.node}
        spec["tolerations"] = [{"key": "node-role.kubernetes.io/control-plane", "operator": "Exists", "effect": "NoSchedule"}]
    kube("create", "-f", "-", data=json.dumps(job))
    print("Started " + namespace + "/" + name, flush=True)
    deadline = time.monotonic() + 360
    while time.monotonic() < deadline:
        state = json.loads(kube("-n", namespace, "get", "job", name, "-o", "json"))["status"]
        if state.get("succeeded") or state.get("failed"):
            print(kube("-n", namespace, "logs", "job/" + name), end="")
            if not state.get("succeeded"):
                raise SystemExit(1)
            return
        time.sleep(2)
    print(kube("-n", namespace, "logs", "job/" + name), end="")
    raise SystemExit("Smoke job timed out; inspect " + name)


if __name__ == "__main__":
    main()
