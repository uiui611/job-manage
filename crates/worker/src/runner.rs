use crate::config::{Config, MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_FILE_BYTES};
use crate::process::ProcessTree;
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use job_protocol::{
    Assignment, Driver, MAX_LOG_BYTES, MAX_RESULT_BYTES, Outcome, WorkerMessage, safe_file_name,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use uuid::Uuid;

pub struct UploadRequest {
    pub request_id: String,
    pub attempt_id: String,
    pub name: String,
    pub size: u64,
    pub reply: oneshot::Sender<UploadLocation>,
}

pub struct UploadLocation {
    pub artifact_id: String,
    pub url: String,
}

#[derive(Clone)]
pub struct RunContext {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub log_tx: mpsc::Sender<WorkerMessage>,
    pub upload_tx: mpsc::Sender<UploadRequest>,
}

pub struct RunReport {
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub artifact_ids: Vec<String>,
    pub logs_truncated: bool,
}

impl RunReport {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed,
            exit_code: None,
            result: None,
            error: Some(message.into()),
            artifact_ids: vec![],
            logs_truncated: false,
        }
    }
    pub fn into_message(self, attempt_id: String) -> WorkerMessage {
        WorkerMessage::Finished {
            attempt_id,
            outcome: self.outcome,
            exit_code: self.exit_code,
            result: self.result,
            error: self.error,
            artifact_ids: self.artifact_ids,
        }
    }
}

pub async fn probe_drivers(config: &Config) -> Vec<Driver> {
    let mut drivers = Vec::new();
    for (driver, runtime) in [
        (Driver::Node, &config.node_path),
        (Driver::Python, &config.python_path),
    ] {
        let mut command = Command::new(runtime);
        command
            .args(match driver {
                Driver::Node => ["--eval", "process.stdout.write('job-runtime-probe')"],
                Driver::Python => ["-c", "print('job-runtime-probe', end='')"],
            })
            .env_clear()
            .envs(&config.runtime_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok((mut child, group)) = ProcessTree::spawn(&mut command) {
            let Some(stdout) = child.stdout.take() else {
                continue;
            };
            let probe = async {
                let wait = async {
                    let status = child.wait().await;
                    drop(group);
                    status
                };
                let read = async {
                    let mut bytes = Vec::new();
                    stdout.take(64).read_to_end(&mut bytes).await?;
                    Ok::<_, std::io::Error>(bytes)
                };
                let (status, output) = tokio::join!(wait, read);
                Ok::<_, std::io::Error>(status?.success() && output? == b"job-runtime-probe")
            };
            if matches!(
                tokio::time::timeout(Duration::from_secs(5), probe).await,
                Ok(Ok(true))
            ) {
                drivers.push(driver);
            }
        }
    }
    drivers
}

pub async fn run_attempt(
    context: RunContext,
    assignment: Assignment,
    cancel: CancellationToken,
) -> RunReport {
    // Never use a server-supplied identifier as a filesystem path.
    let workspace = context
        .config
        .work_dir
        .join(format!("attempt-{}", Uuid::new_v4()));
    let logs_truncated = Arc::new(AtomicBool::new(false));
    let execution = execute(&context, &assignment, &workspace, logs_truncated.clone());
    let mut report = tokio::select! {
        biased;
        _ = cancel.cancelled() => RunReport {
            outcome: Outcome::Canceled, exit_code: None, result: None,
            error: Some("execution canceled".into()), artifact_ids: vec![], logs_truncated: false,
        },
        _ = tokio::time::sleep(Duration::from_secs(assignment.definition.timeout_seconds.max(1))) => {
            RunReport::failed("execution deadline exceeded")
        },
        result = execution => match result {
            Ok(report) => report,
            Err(error) => RunReport::failed(error.to_string()),
        }
    };
    report.logs_truncated = logs_truncated.load(Ordering::Relaxed);
    // Dropping execute first stops the process tree, including children holding pipes.
    // Only the UUID directory created for this attempt is ever removed.
    if let Err(error) = remove_workspace(&workspace).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            os_error = error.raw_os_error(),
            "could not remove an attempt workspace"
        );
    }
    report
}

async fn execute(
    context: &RunContext,
    assignment: &Assignment,
    workspace: &Path,
    logs_truncated: Arc<AtomicBool>,
) -> Result<RunReport> {
    fs::create_dir(workspace)
        .await
        .context("cannot create attempt workspace")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(workspace, std::fs::Permissions::from_mode(0o700))
            .await
            .context("cannot restrict attempt workspace permissions")?;
    }
    let inputs = workspace.join("inputs");
    let artifacts = workspace.join("artifacts");
    let temp = workspace.join("tmp");
    for directory in [&inputs, &artifacts, &temp] {
        fs::create_dir(directory)
            .await
            .context("cannot prepare attempt directories")?;
    }
    let input_path = workspace.join("input.json");
    let result_path = workspace.join("result.json");
    fs::write(&input_path, serde_json::to_vec(&assignment.input)?)
        .await
        .context("cannot write JOB input")?;
    download_inputs(context, assignment, &inputs).await?;
    let (runtime, script_name) = match assignment.definition.driver {
        Driver::Node => (&context.config.node_path, "main.mjs"),
        Driver::Python => (&context.config.python_path, "main.py"),
    };
    let script = workspace.join(script_name);
    fs::write(&script, &assignment.definition.script)
        .await
        .context("cannot write JOB script")?;
    let mut command = Command::new(runtime);
    command
        // Launch within the workspace: Node's entry-point resolver rejects some
        // Windows verbatim absolute paths returned by canonicalize.
        .arg(script_name)
        .current_dir(workspace)
        .env_clear()
        .envs(&context.config.runtime_env)
        .env("HOME", workspace)
        .env("TMPDIR", &temp)
        .env("LANG", "C.UTF-8")
        .env("PYTHONUNBUFFERED", "1")
        .env("JOB_INPUT_FILE", &input_path)
        .env("JOB_RESULT_FILE", &result_path)
        .env("JOB_INPUT_DIR", &inputs)
        .env("JOB_ARTIFACT_DIR", &artifacts)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command
        .env("USERPROFILE", workspace)
        .env("TEMP", &temp)
        .env("TMP", &temp);
    let (mut child, group) = ProcessTree::spawn(&mut command)?;
    let stdout = child.stdout.take().context("stdout pipe unavailable")?;
    let stderr = child.stderr.take().context("stderr pipe unavailable")?;
    let budget = Arc::new(AtomicUsize::new(MAX_LOG_BYTES));
    let stdout_read = drain_logs(
        stdout,
        context.log_tx.clone(),
        assignment.attempt_id.clone(),
        "stdout",
        budget.clone(),
        logs_truncated.clone(),
    );
    let stderr_read = drain_logs(
        stderr,
        context.log_tx.clone(),
        assignment.attempt_id.clone(),
        "stderr",
        budget,
        logs_truncated,
    );
    let wait_child = async {
        let status = child.wait().await;
        // A script must not leave descendants behind after its main process exits.
        drop(group);
        status
    };
    let (status, (), ()) = tokio::join!(wait_child, stdout_read, stderr_read);
    let status = status.context("cannot wait for runtime")?;
    if !status.success() {
        return Ok(RunReport {
            outcome: Outcome::Failed,
            exit_code: status.code(),
            result: None,
            error: Some("runtime exited unsuccessfully".into()),
            artifact_ids: vec![],
            logs_truncated: false,
        });
    }
    let result = read_result(&result_path).await?;
    let artifact_ids = upload_artifacts(context, assignment, &artifacts).await?;
    Ok(RunReport {
        outcome: Outcome::Succeeded,
        exit_code: status.code(),
        result,
        error: None,
        artifact_ids,
        logs_truncated: false,
    })
}

async fn download_inputs(
    context: &RunContext,
    assignment: &Assignment,
    directory: &Path,
) -> Result<()> {
    ensure!(assignment.files.len() <= MAX_FILES, "too many input files");
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for input in &assignment.files {
        ensure!(
            safe_file_name(&input.name) && seen.insert(&input.name),
            "invalid or duplicate input filename"
        );
        ensure!(input.size <= MAX_FILE_BYTES, "input file exceeds 64 MiB");
        total = total
            .checked_add(input.size)
            .context("input size overflow")?;
        ensure!(
            total <= MAX_TOTAL_FILE_BYTES,
            "input files exceed 512 MiB in total"
        );
        let url = context.config.transfer_url(&input.url)?;
        // These URLs carry their own short-lived signature. Never add the worker token.
        let response = context
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("input download transport failed"))?;
        ensure!(
            response.status().is_success(),
            "input download returned a non-success status"
        );
        if let Some(length) = response.content_length() {
            ensure!(
                length == input.size,
                "input download size does not match assignment"
            );
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(&input.name))
            .await
            .context("cannot create input file")?;
        let mut received = 0u64;
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("input download interrupted"))?;
            received = received
                .checked_add(chunk.len() as u64)
                .context("input size overflow")?;
            ensure!(
                received <= input.size && received <= MAX_FILE_BYTES,
                "input download exceeds declared size"
            );
            output
                .write_all(&chunk)
                .await
                .context("cannot write input file")?;
        }
        ensure!(received == input.size, "input download is incomplete");
        output.flush().await.context("cannot flush input file")?;
    }
    Ok(())
}

async fn read_result(path: &Path) -> Result<Option<Value>> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot inspect result.json"),
    };
    ensure!(
        regular_file(&metadata),
        "result.json must be a regular file, not a symlink"
    );
    ensure!(
        metadata.len() <= MAX_RESULT_BYTES as u64,
        "result.json exceeds 1 MiB"
    );
    let file = fs::File::open(path)
        .await
        .context("cannot open result.json")?;
    let mut bytes = Vec::new();
    file.take(MAX_RESULT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .context("cannot read result.json")?;
    ensure!(bytes.len() <= MAX_RESULT_BYTES, "result.json exceeds 1 MiB");
    Ok(Some(
        serde_json::from_slice(&bytes).context("result.json must contain valid JSON")?,
    ))
}

async fn upload_artifacts(
    context: &RunContext,
    assignment: &Assignment,
    directory: &Path,
) -> Result<Vec<String>> {
    let mut entries = fs::read_dir(directory)
        .await
        .context("cannot list artifacts")?;
    let mut artifacts: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut total = 0u64;
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("cannot read artifact entry")?
    {
        ensure!(artifacts.len() < MAX_FILES, "more than 32 artifacts");
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("artifact filename is not UTF-8"))?;
        ensure!(safe_file_name(&name), "invalid artifact filename");
        let metadata = fs::symlink_metadata(entry.path())
            .await
            .context("cannot inspect artifact")?;
        ensure!(
            regular_file(&metadata),
            "artifacts must be regular files, without symlinks or subdirectories"
        );
        ensure!(metadata.len() <= MAX_FILE_BYTES, "artifact exceeds 64 MiB");
        total = total
            .checked_add(metadata.len())
            .context("artifact size overflow")?;
        ensure!(
            total <= MAX_TOTAL_FILE_BYTES,
            "artifacts exceed 512 MiB in total"
        );
        artifacts.push((name, entry.path(), metadata.len()));
    }
    artifacts.sort_by(|left, right| left.0.cmp(&right.0));
    let mut ids = Vec::new();
    for (name, path, size) in artifacts {
        let (reply, receive) = oneshot::channel();
        context
            .upload_tx
            .send(UploadRequest {
                request_id: Uuid::new_v4().to_string(),
                attempt_id: assignment.attempt_id.clone(),
                name,
                size,
                reply,
            })
            .await
            .context("server connection closed before artifact upload")?;
        let location = tokio::time::timeout(Duration::from_secs(30), receive)
            .await
            .context("artifact upload authorization timed out")?
            .context("server connection closed before artifact upload")?;
        let url = context.config.transfer_url(&location.url)?;
        let file = fs::File::open(path).await.context("cannot open artifact")?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file.take(size)));
        let response = context
            .http
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("artifact upload transport failed"))?;
        ensure!(
            response.status().is_success(),
            "artifact upload returned a non-success status"
        );
        ids.push(location.artifact_id);
    }
    Ok(ids)
}

async fn drain_logs<R: AsyncRead + Unpin>(
    mut reader: R,
    sender: mpsc::Sender<WorkerMessage>,
    attempt_id: String,
    stream: &'static str,
    budget: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let mut text = String::from_utf8_lossy(&buffer[..count]).into_owned();
        let reserved = reserve_bytes(&budget, text.len());
        let mut end = reserved;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            truncated.store(true, Ordering::Relaxed);
        }
        text.truncate(end);
        if !text.is_empty() {
            // A full log queue discards output, but never blocks process pipes or heartbeats.
            if sender
                .try_send(WorkerMessage::Log {
                    attempt_id: attempt_id.clone(),
                    stream: stream.into(),
                    text,
                })
                .is_err()
            {
                truncated.store(true, Ordering::Relaxed);
            }
        }
    }
}

fn reserve_bytes(budget: &AtomicUsize, wanted: usize) -> usize {
    let mut available = budget.load(Ordering::Relaxed);
    loop {
        let reserved = available.min(wanted);
        match budget.compare_exchange_weak(
            available,
            available - reserved,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return reserved,
            Err(updated) => available = updated,
        }
    }
}

fn regular_file(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject all reparse points, including symlinks and Windows-specific redirection.
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    metadata.file_type().is_file()
}

async fn remove_workspace(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    for _ in 0..40 {
        // Job termination is asynchronous; let killed processes release file handles.
        match fs::remove_dir_all(path).await {
            Err(error) if matches!(error.raw_os_error(), Some(5 | 16 | 32 | 33 | 145 | 303)) => {
                // Access denied, current directory, sharing/lock violation, directory
                // not empty, or delete pending can occur while Windows releases handles.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A child entry disappearing during deletion need not mean the UUID
                // workspace itself has disappeared. Retry only that same owned directory.
                if fs::symlink_metadata(path).await.is_err() {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
    fs::remove_dir_all(path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use job_protocol::DefinitionSpec;

    #[tokio::test]
    async fn bounded_logs_do_not_wait_for_a_slow_consumer() {
        let (sender, mut receiver) = mpsc::channel(1);
        let bytes = vec![b'x'; MAX_LOG_BYTES + 1024];
        let budget = Arc::new(AtomicUsize::new(MAX_LOG_BYTES));
        let truncated = Arc::new(AtomicBool::new(false));
        tokio::time::timeout(
            Duration::from_secs(2),
            drain_logs(
                bytes.as_slice(),
                sender,
                "attempt".into(),
                "stdout",
                budget.clone(),
                truncated.clone(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(budget.load(Ordering::Relaxed), 0);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        assert!(truncated.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn result_limit_is_enforced() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("result.json");
        assert!(read_result(&path).await.unwrap().is_none());
        fs::write(&path, vec![b' '; MAX_RESULT_BYTES + 1])
            .await
            .unwrap();
        assert!(read_result(&path).await.is_err());
        fs::write(&path, b"{\"ok\":true}").await.unwrap();
        assert_eq!(
            read_result(&path).await.unwrap(),
            Some(serde_json::json!({"ok": true}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_symlink_results() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("secret");
        let path = temporary.path().join("result.json");
        fs::write(&target, b"null").await.unwrap();
        std::os::unix::fs::symlink(target, &path).unwrap();
        assert!(read_result(&path).await.is_err());
    }

    #[cfg(any(target_os = "linux", windows))]
    async fn process_tree_case(cancel_early: bool) {
        let temporary = tempfile::tempdir().unwrap();
        let pid_file = temporary.path().join("child.pid");
        let config = test_config(temporary.path().to_path_buf());
        let (log_tx, _) = mpsc::channel(2);
        let (upload_tx, _) = mpsc::channel(2);
        let context = RunContext {
            config: Arc::new(config),
            http: reqwest::Client::new(),
            log_tx,
            upload_tx,
        };
        let script = format!(
            "import subprocess,time,pathlib,sys\np = subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'])\npathlib.Path({:?}).write_text(str(p.pid))\ntime.sleep(60)\n",
            pid_file.to_str().unwrap()
        );
        let assignment = Assignment {
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            input: serde_json::json!({}),
            files: vec![],
            definition: DefinitionSpec {
                name: "tree".into(),
                description: "".into(),
                driver: Driver::Python,
                script,
                input_schema: serde_json::json!({}),
                required_tags: vec![],
                timeout_seconds: if cancel_early { 30 } else { 2 },
            },
        };
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_attempt(context, assignment, cancel.clone()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("python3 must be installed for worker process tests");
        let pid: u32 = fs::read_to_string(&pid_file)
            .await
            .unwrap()
            .parse()
            .unwrap();
        if cancel_early {
            cancel.cancel();
        }
        let report = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            report.outcome,
            if cancel_early {
                Outcome::Canceled
            } else {
                Outcome::Failed
            }
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !crate::process::is_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("descendant survived cancel or timeout");
        let mut directories = fs::read_dir(temporary.path()).await.unwrap();
        while let Some(entry) = directories.next_entry().await.unwrap() {
            assert!(!entry.file_name().to_string_lossy().starts_with("attempt-"));
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn cancellation_kills_the_process_tree() {
        process_tree_case(true).await;
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn timeout_kills_the_process_tree() {
        process_tree_case(false).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn installed_runtimes_execute_in_unicode_and_space_paths() {
        for driver in [Driver::Node, Driver::Python] {
            let temporary = tempfile::tempdir().unwrap();
            let work = temporary.path().join("JOB 日本語 workspace");
            fs::create_dir(&work).await.unwrap();
            let config = test_config(fs::canonicalize(&work).await.unwrap());
            let (log_tx, mut logs) = mpsc::channel(8);
            let (upload_tx, _) = mpsc::channel(2);
            let script = match driver {
                Driver::Node => {
                    "import fs from 'node:fs'; const input=JSON.parse(fs.readFileSync(process.env.JOB_INPUT_FILE,'utf8')); if(process.env.JOB_WORKER_TOKEN) throw Error('credential inherited'); fs.writeFileSync(process.env.JOB_RESULT_FILE,JSON.stringify({sum:input.a+input.b})); console.log('node-ok');"
                }
                Driver::Python => {
                    "import os,json,pathlib\nassert 'JOB_WORKER_TOKEN' not in os.environ\nassert os.environ['TEMP']==os.environ['TMP']\nassert os.environ['SystemRoot']\ndata=json.loads(pathlib.Path(os.environ['JOB_INPUT_FILE']).read_text())\npathlib.Path(os.environ['JOB_RESULT_FILE']).write_text(json.dumps({'sum':data['a']+data['b']}))\nprint('python-ok')\n"
                }
            };
            let assignment = Assignment {
                run_id: "run".into(),
                attempt_id: "attempt".into(),
                input: serde_json::json!({"a": 2, "b": 3}),
                files: vec![],
                definition: DefinitionSpec {
                    name: "native".into(),
                    description: String::new(),
                    driver,
                    script: script.into(),
                    input_schema: serde_json::json!({}),
                    required_tags: vec![],
                    timeout_seconds: 15,
                },
            };
            let report = run_attempt(
                RunContext {
                    config: Arc::new(config),
                    http: reqwest::Client::new(),
                    log_tx,
                    upload_tx,
                },
                assignment,
                CancellationToken::new(),
            )
            .await;
            assert_eq!(report.outcome, Outcome::Succeeded, "{:?}", report.error);
            assert_eq!(report.result, Some(serde_json::json!({"sum":5})));
            assert!(
                matches!(logs.try_recv(), Ok(WorkerMessage::Log { text, .. }) if text.contains("-ok"))
            );
            assert!(
                fs::read_dir(&work)
                    .await
                    .unwrap()
                    .next_entry()
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn probes_only_available_runtimes() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = test_config(temporary.path().to_path_buf());
        config.node_path = temporary
            .path()
            .join("not-installed.exe")
            .to_string_lossy()
            .into_owned();
        assert_eq!(probe_drivers(&config).await, vec![Driver::Python]);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn successful_non_runtime_executable_is_not_registered() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = test_config(temporary.path().to_path_buf());
        let executable = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("hostname.exe");
        config.node_path = executable.to_string_lossy().into_owned();
        config.python_path = config.node_path.clone();
        assert!(probe_drivers(&config).await.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn normal_exit_stops_descendants_holding_pipes() {
        let temporary = tempfile::tempdir().unwrap();
        let pid_file = temporary.path().join("child.pid");
        let config = test_config(temporary.path().to_path_buf());
        let (log_tx, _) = mpsc::channel(8);
        let (upload_tx, _) = mpsc::channel(2);
        let assignment = Assignment {
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            input: serde_json::json!({}),
            files: vec![],
            definition: DefinitionSpec {
                name: "exit".into(),
                description: String::new(),
                driver: Driver::Python,
                script: format!(
                    "import subprocess,pathlib,sys\np=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'])\npathlib.Path({:?}).write_text(str(p.pid))\n",
                    pid_file.to_str().unwrap()
                ),
                input_schema: serde_json::json!({}),
                required_tags: vec![],
                timeout_seconds: 10,
            },
        };
        let report = run_attempt(
            RunContext {
                config: Arc::new(config),
                http: reqwest::Client::new(),
                log_tx,
                upload_tx,
            },
            assignment,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(report.outcome, Outcome::Succeeded, "{:?}", report.error);
        let pid = fs::read_to_string(pid_file).await.unwrap().parse().unwrap();
        assert!(!crate::process::is_alive(pid));
    }
}
