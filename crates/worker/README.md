# Linux / Windows worker

This release runs one job at a time in an existing Linux container/VM or directly on Windows. The
agent directly spawns Node or Python; it does not need the Docker socket and does
not create containers. Windows uses installed Node/Python runtimes and Job Objects
for process-tree lifetime control. See [Windows setup](../../docs/windows-worker.md).

## Configuration

| Variable | Default / meaning |
| --- | --- |
| `JOB_SERVER_URL` | Required. HTTPS base URL, such as `https://host/jobs`. |
| `JOB_WORKER_TOKEN` | Required. Worker authentication token. |
| `JOB_WORKER_ID` | Required. Stable unique ASCII identifier using letters, digits, `_`, `-`, `.` (not `.` or `..`), up to 120 bytes. Never share it between concurrent agents. |
| `JOB_WORKER_NAME` | Worker ID. Human-readable display name, 1–160 bytes. |
| `JOB_WORKER_TAGS` | `[]`. JSON array, e.g. `["os=linux","env=base"]`. Maximum 64 tags, 128 bytes each. |
| `JOB_WORK_DIR` | `/work` on Linux; `%LOCALAPPDATA%\JobManage\work` on Windows. Writable workspace root; converted to an absolute path at startup. |
| `JOB_NODE_PATH` | `node`. Node executable, preferably an absolute path if customized. |
| `JOB_PYTHON_PATH` | `python3` on Linux, `python` on Windows. Python executable, preferably an absolute path if customized. |
| `JOB_ALLOW_INSECURE_HTTP` | `false`. Only literal `true` permits development HTTP/WS. TLS certificate checks remain enabled for HTTPS/WSS. |
| `JOB_CA_CERT_FILE` | Optional path to a PEM bundle of additional trusted CA certificates for both HTTPS file transfers and WSS. Standard roots and hostname verification remain enabled. |
| `JOB_RUNTIME_ENV_ALLOWLIST` | Optional comma-separated extra environment variable names to pass to scripts. Names beginning with `JOB_` are forbidden. |

Runtime availability is probed with `--version`. At least one runtime must work.
Scripts receive a cleared environment containing `PATH`, per-attempt `HOME` and
`TMPDIR`, `LANG=C.UTF-8`, `PYTHONUNBUFFERED=1`, and the job path variables below.
GPU settings are also copied when present: `LD_LIBRARY_PATH`,
`CUDA_VISIBLE_DEVICES`, `NVIDIA_VISIBLE_DEVICES`, `NVIDIA_DRIVER_CAPABILITIES`,
`ROCR_VISIBLE_DEVICES`, and `HIP_VISIBLE_DEVICES`. Other application-specific
settings require the explicit allowlist. The agent's authentication variables
are never inherited by its child process.

For a private CA, mount its public certificate bundle read-only and set
`JOB_CA_CERT_FILE=/etc/job-manage/ca.pem`. This trusts server certificates; it is
not a mutual-TLS client certificate setting. Do not mount the CA private key. The
file is read once at startup, must contain at least one valid PEM certificate,
and must be no larger than 1 MiB. An unreadable, empty or invalid bundle fails
startup instead of falling back to an insecure connection. Restart the worker
after changing the bundle. This setting is not passed to job scripts and does
not configure TLS clients inside those scripts.

## Script contract

- `JOB_INPUT_FILE`: UTF-8 JSON input file, containing the submitted parameters.
- `JOB_INPUT_DIR`: downloaded input files, with the names supplied at submission.
- `JOB_RESULT_FILE`: write UTF-8 JSON here for a structured result. Missing file
  means a successful job has a null result. Invalid JSON or more than 1 MiB fails.
- `JOB_ARTIFACT_DIR`: place result files directly in this directory. Filenames
  allow only ASCII letters, digits, `_`, `-`, and `.`, with maximum 180 bytes.
  Symlinks and subdirectories are rejected. At most 32 files, 64 MiB each and
  512 MiB total; the same limits apply separately to input files.

Node scripts are saved as `main.mjs` (ES modules); Python scripts as `main.py`.
Dependencies should already exist in the worker image. Standard output/error are
streamed as best-effort bounded chunks, with a combined 2 MiB limit per attempt.
A full log queue discards output to keep process pipes and heartbeats responsive.
Before completion, the worker reports a truncation marker so the API/UI can show
that output was omitted, even when the server itself received less than 2 MiB.
Success is reported after all artifacts have been uploaded. Nonzero runtime exit
status fails the job. The configured job deadline includes input preparation,
execution, and result uploads. It does not include waiting in the server queue.

The worker uses one authenticated WebSocket under `<base>/worker`, plus separate
HTTPS file transfers with server-issued signed URLs. File requests do not carry
the worker token, never follow redirects, and enforce the declared input size.
Completion holds the single execution slot until the server acknowledges it.

Disconnect, server silence, cancellation, timeout, and worker shutdown stop the
Linux process group or Windows Job Object, including descendants. A completed
main process also has its remaining descendants killed. The failed or interrupted
attempt is never resumed after reconnect; the worker reconnects with a 1–30 second
exponential backoff. Workspaces are removed on normal completion/cancellation;
SIGKILL, power loss or a filesystem error can leave an `attempt-<UUID>` directory
for manual cleanup.

## Isolation boundary

Scripts are trusted code supplied by the operator. A shared container and Unix
process group are not a hostile-code sandbox: scripts can inspect processes under
the same user, modify shared resources, or deliberately detach into a new session.
Environment filtering prevents accidental credential inheritance, not deliberate
credential theft by hostile code. Run the worker as a non-root user in a dedicated
container/VM, avoid host-directory and Docker-socket mounts, and set container CPU,
RAM and storage limits. GPU access and frameworks must be provisioned for the
actual GPU vendor and runtime; tags do not measure available VRAM.

Linux worker tests require `python3`; Windows tests require Node and Python from the environment. They cover process-tree cancellation,
timeout, WebSocket disconnection and a server that stops responding, as well as
bounded logs, result size, symlink rejection and transfer URL validation.
TLS tests generate temporary certificates at runtime and verify HTTPS and WSS
trust, rejection without the extra CA, hostname checks, standard root retention,
and invalid CA configuration. No deployment certificates or private keys are
included in the repository.
