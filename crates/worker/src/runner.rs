use crate::config::{Config, MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_FILE_BYTES};
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
            .arg("--version")
            .env_clear()
            .envs(&config.runtime_env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        set_process_group(&mut command);
        if let Ok(mut child) = command.spawn() {
            let group = ProcessGroup::new(child.id());
            if matches!(tokio::time::timeout(Duration::from_secs(5), child.wait()).await, Ok(Ok(status)) if status.success())
            {
                drivers.push(driver);
            }
            drop(group);
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
    // Dropping execute first kills the entire process group, including children holding pipes.
    // Only the UUID directory created for this attempt is ever removed.
    if let Err(error) = fs::remove_dir_all(&workspace).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("could not remove an attempt workspace");
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
        .arg(&script)
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
    set_process_group(&mut command);
    let mut child = command.spawn().context("cannot start selected runtime")?;
    let group = ProcessGroup::new(child.id());
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
        metadata.file_type().is_file(),
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
            metadata.file_type().is_file(),
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

fn set_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

struct ProcessGroup {
    pid: Option<u32>,
}
impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            // The process was made leader of a new group at spawn; no shell is involved.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
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

    #[cfg(unix)]
    async fn process_tree_case(cancel_early: bool) {
        let temporary = tempfile::tempdir().unwrap();
        let pid_file = temporary.path().join("child.pid");
        let mut config = test_config(temporary.path().to_path_buf());
        config.python_path = "python3".into();
        let (log_tx, _) = mpsc::channel(2);
        let (upload_tx, _) = mpsc::channel(2);
        let context = RunContext {
            config: Arc::new(config),
            http: reqwest::Client::new(),
            log_tx,
            upload_tx,
        };
        let script = format!(
            "import subprocess,time,pathlib\np = subprocess.Popen(['python3','-c','import time; time.sleep(60)'])\npathlib.Path({:?}).write_text(str(p.pid))\ntime.sleep(60)\n",
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
        let pid: i32 = fs::read_to_string(&pid_file)
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
                // A zombie is already dead; its adopting parent determines when it is reaped.
                let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                    .await
                    .unwrap_or_default();
                if state.is_empty()
                    || state
                        .split(") ")
                        .nth(1)
                        .is_some_and(|tail| tail.starts_with('Z'))
                {
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_process_tree() {
        process_tree_case(true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_process_tree() {
        process_tree_case(false).await;
    }
}
