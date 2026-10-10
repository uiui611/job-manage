mod config;
mod process;
mod runner;
mod tls;

use anyhow::{Context, Result, bail, ensure};
use config::Config;
use futures_util::{SinkExt, StreamExt};
use job_protocol::{Driver, PROTOCOL_VERSION, ServerMessage, VERSION, WorkerMessage};
use runner::{RunContext, RunReport, UploadLocation};
use std::{collections::HashMap, future::pending, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct ActiveAttempt {
    attempt_id: String,
    cancel: CancellationToken,
    task: Option<JoinHandle<RunReport>>,
    finished_at: Option<Instant>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => {}
        [arg] if arg == "--version" => {
            println!("job-worker {VERSION}");
            return Ok(());
        }
        [arg] if arg == "--help" || arg == "-h" => {
            println!(
                "job-worker {VERSION}\n\nRuns Node.js/Python JOBs using installed runtimes.\nConfigure JOB_SERVER_URL, JOB_WORKER_TOKEN and JOB_WORKER_ID via environment.\nOptional: JOB_NODE_PATH, JOB_PYTHON_PATH, JOB_WORK_DIR, JOB_WORKER_TAGS, JOB_CA_CERT_FILE.\nStop with Ctrl+C. See docs/windows-worker.md for Windows setup."
            );
            return Ok(());
        }
        _ => bail!("unknown argument; use --help"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "job_worker=info".into()),
        )
        .init();
    ensure!(
        cfg!(any(target_os = "linux", target_os = "windows")),
        "the worker supports Linux and Windows"
    );
    process::initialize_worker()?;
    let mut config = Config::from_env()?;
    // Load additional trust once and fail before connecting if it is invalid.
    let transport = tls::Transport::new(config.ca_cert_file.as_deref())?;
    tokio::fs::create_dir_all(&config.work_dir)
        .await
        .context("cannot create JOB_WORK_DIR")?;
    config.work_dir = tokio::fs::canonicalize(&config.work_dir)
        .await
        .context("cannot resolve JOB_WORK_DIR")?;
    let drivers = runner::probe_drivers(&config).await;
    ensure!(
        !drivers.is_empty(),
        "no working Node or Python runtime found"
    );
    let config = Arc::new(config);
    let shutdown = CancellationToken::new();
    let signal_cancel = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        signal_cancel.cancel();
    });
    tracing::info!(version = VERSION, "worker started; ready to connect");
    let mut backoff = 1u64;
    loop {
        let began = Instant::now();
        let result = run_session(
            config.clone(),
            transport.http.clone(),
            transport.websocket.clone(),
            drivers.clone(),
            shutdown.clone(),
        )
        .await;
        if shutdown.is_cancelled() {
            break;
        }
        // Do not log HTTP client errors: they can contain presigned URLs or response details.
        if result.is_err() {
            tracing::warn!("server connection ended; active work has been stopped");
        }
        if began.elapsed() >= Duration::from_secs(60) {
            backoff = 1;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {},
        }
        backoff = (backoff * 2).min(30);
    }
    tracing::info!("worker stopped");
    Ok(())
}

async fn run_session(
    config: Arc<Config>,
    http: reqwest::Client,
    connector: Option<tokio_tungstenite::Connector>,
    drivers: Vec<Driver>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut request = config
        .websocket_url()?
        .as_str()
        .into_client_request()
        .context("cannot build worker connection request")?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", config.token)
            .parse()
            .context("invalid worker token format")?,
    );
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(4 * 1024 * 1024))
        .max_frame_size(Some(4 * 1024 * 1024));
    let (mut socket, _) = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        connected = tokio::time::timeout(Duration::from_secs(15),
            tokio_tungstenite::connect_async_tls_with_config(request, Some(websocket_config), false, connector)) => {
            connected.context("server connection timed out")?.context("cannot connect to server")?
        },
    };
    send(
        &mut socket,
        WorkerMessage::Hello {
            worker_id: config.worker_id.clone(),
            name: config.name.clone(),
            version: VERSION.into(),
            protocol_version: PROTOCOL_VERSION,
            tags: config.tags.clone(),
            drivers: drivers.clone(),
        },
    )
    .await?;
    let welcome = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(15), receive_welcome(&mut socket)) => {
            result.context("server handshake timed out")??
        },
    };
    let (heartbeat_seconds, heartbeat_timeout_seconds) = welcome;
    tracing::info!("authenticated worker session established");
    let (log_tx, mut log_rx) = mpsc::channel(32);
    let (upload_tx, mut upload_rx) = mpsc::channel(4);
    let context = RunContext {
        config,
        http,
        log_tx,
        upload_tx,
    };
    let mut active: Option<ActiveAttempt> = None;
    let mut uploads: HashMap<String, oneshot::Sender<UploadLocation>> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_seconds));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watchdog = tokio::time::interval(Duration::from_secs(1));
    watchdog.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_received = Instant::now();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Ok(()),
                _ = watchdog.tick() => {
                    ensure!(last_received.elapsed() < Duration::from_secs(heartbeat_timeout_seconds), "server became unresponsive");
                    if active.as_ref().and_then(|run| run.finished_at).is_some_and(|finished| finished.elapsed() > Duration::from_secs(30)) {
                        bail!("server did not acknowledge completion");
                    }
                },
                _ = heartbeat.tick() => {
                    send(&mut socket, WorkerMessage::Heartbeat {}).await?;
                    send_frame(&mut socket, Message::Ping(Vec::new().into())).await?;
                },
                incoming = socket.next() => {
                    let message = incoming.context("server disconnected")?.context("worker connection failed")?;
                    last_received = Instant::now();
                    match message {
                        Message::Text(text) => {
                            let message: ServerMessage = serde_json::from_str(text.as_str()).context("invalid server message")?;
                            match message {
                                ServerMessage::Assign { assignment } => {
                                    ensure!(active.is_none(), "server assigned work to a busy worker");
                                    ensure!(drivers.contains(&assignment.definition.driver), "server assigned an unavailable runtime");
                                    let cancel = CancellationToken::new();
                                    let attempt_id = assignment.attempt_id.clone();
                                    let task = tokio::spawn(runner::run_attempt(context.clone(), assignment, cancel.clone()));
                                    active = Some(ActiveAttempt { attempt_id: attempt_id.clone(), cancel, task: Some(task), finished_at: None });
                                    send(&mut socket, WorkerMessage::Started { attempt_id }).await?;
                                },
                                ServerMessage::Cancel { attempt_id } => {
                                    if let Some(run) = active.as_ref().filter(|run| run.attempt_id == attempt_id) { run.cancel.cancel(); }
                                },
                                ServerMessage::UploadUrl { request_id, artifact_id, url } => {
                                    if let Some(reply) = uploads.remove(&request_id) {
                                        let _ = reply.send(UploadLocation { artifact_id, url });
                                    }
                                },
                                ServerMessage::FinishedAck { attempt_id, accepted } => {
                                    let run = active.as_ref().context("unexpected completion acknowledgement")?;
                                    ensure!(run.attempt_id == attempt_id && run.task.is_none(), "invalid completion acknowledgement");
                                    if !accepted { tracing::warn!("server rejected stale attempt completion; it will not be replayed"); }
                                    active = None;
                                    uploads.clear();
                                },
                                ServerMessage::Error { .. } => bail!("server rejected a worker operation"),
                                ServerMessage::Welcome { .. } => bail!("unexpected second handshake"),
                            }
                        },
                        Message::Ping(bytes) => send_frame(&mut socket, Message::Pong(bytes)).await?,
                        Message::Pong(_) => {},
                        Message::Close(_) => bail!("server closed worker session"),
                        _ => bail!("unexpected server frame"),
                    }
                },
                report = async {
                    if let Some(task) = active.as_mut().and_then(|run| run.task.as_mut()) { task.await }
                    else { pending::<std::result::Result<RunReport, tokio::task::JoinError>>().await }
                }, if log_rx.is_empty() => {
                    let report = report.unwrap_or_else(|_| RunReport::failed("worker execution task stopped unexpectedly"));
                    let run = active.as_mut().context("execution completed without active attempt")?;
                    run.task = None;
                    run.finished_at = Some(Instant::now());
                    uploads.clear();
                    if report.logs_truncated {
                        send(&mut socket, WorkerMessage::LogTruncated { attempt_id: run.attempt_id.clone() }).await?;
                    }
                    send(&mut socket, report.into_message(run.attempt_id.clone())).await?;
                },
                Some(upload) = upload_rx.recv() => {
                    if active.as_ref().is_some_and(|run| run.task.is_some() && run.attempt_id == upload.attempt_id) {
                        let message = WorkerMessage::RequestUpload {
                            request_id: upload.request_id.clone(), attempt_id: upload.attempt_id,
                            name: upload.name, size: upload.size,
                        };
                        uploads.insert(upload.request_id, upload.reply);
                        send(&mut socket, message).await?;
                    }
                },
                Some(log) = log_rx.recv() => {
                    if let WorkerMessage::Log { attempt_id, .. } = &log
                        && active.as_ref().is_some_and(|run| run.attempt_id == *attempt_id && run.task.is_some())
                    {
                        send(&mut socket, log).await?;
                    }
                },
            }
        }
    }.await;
    // Never carry active attempts into a fresh server session, even if files or results exist.
    uploads.clear();
    if let Some(mut run) = active {
        run.cancel.cancel();
        if let Some(mut task) = run.task.take()
            && tokio::time::timeout(Duration::from_secs(10), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
    result
}

async fn receive_welcome(socket: &mut Socket) -> Result<(u64, u64)> {
    loop {
        match socket
            .next()
            .await
            .context("server closed during handshake")?
            .context("handshake transport failed")?
        {
            Message::Text(text) => match serde_json::from_str::<ServerMessage>(text.as_str())
                .context("invalid server handshake")?
            {
                ServerMessage::Welcome {
                    version,
                    heartbeat_seconds,
                    heartbeat_timeout_seconds,
                    ..
                } => {
                    ensure!(
                        version == VERSION,
                        "server and worker versions must match exactly"
                    );
                    ensure!(
                        (1..=300).contains(&heartbeat_seconds)
                            && heartbeat_timeout_seconds > heartbeat_seconds
                            && heartbeat_timeout_seconds <= 600,
                        "invalid server heartbeat settings"
                    );
                    return Ok((heartbeat_seconds, heartbeat_timeout_seconds));
                }
                ServerMessage::Error { .. } => bail!("server rejected the worker handshake"),
                _ => bail!("server did not send a welcome message"),
            },
            Message::Ping(bytes) => send_frame(socket, Message::Pong(bytes)).await?,
            Message::Pong(_) => {}
            _ => bail!("server closed during handshake"),
        }
    }
}

async fn send(socket: &mut Socket, message: WorkerMessage) -> Result<()> {
    send_frame(
        socket,
        Message::Text(serde_json::to_string(&message)?.into()),
    )
    .await
}

async fn send_frame(socket: &mut Socket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .context("worker connection send timed out")?
        .context("worker connection send failed")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests {
    use super::*;
    use job_protocol::{Assignment, DefinitionSpec};
    use tokio::net::TcpListener;

    async fn lost_session_kills_tree(silent: bool) {
        let temporary = tempfile::tempdir().unwrap();
        let pid_file = temporary.path().join("descendant.pid");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = config::test_config(temporary.path().to_path_buf());
        config.server_url = format!("http://{address}/jobs").parse().unwrap();
        config.allow_http = true;
        let script = format!(
            "import subprocess,pathlib,time,sys\np=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'])\npathlib.Path({:?}).write_text(str(p.pid))\ntime.sleep(60)\n",
            pid_file.to_str().unwrap(),
        );
        let assignment = Assignment {
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            input: serde_json::json!({}),
            files: vec![],
            definition: DefinitionSpec {
                name: "disconnect".into(),
                description: "".into(),
                driver: Driver::Python,
                script,
                input_schema: serde_json::json!({}),
                required_tags: vec![],
                timeout_seconds: 60,
            },
        };
        let server_pid_file = pid_file.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = socket.next().await.unwrap().unwrap();
            assert!(matches!(
                serde_json::from_str::<WorkerMessage>(hello.to_text().unwrap()).unwrap(),
                WorkerMessage::Hello { .. }
            ));
            for message in [
                ServerMessage::Welcome {
                    session_id: "session".into(),
                    heartbeat_seconds: 1,
                    heartbeat_timeout_seconds: 2,
                    version: VERSION.into(),
                },
                ServerMessage::Assign { assignment },
            ] {
                socket
                    .send(Message::Text(
                        serde_json::to_string(&message).unwrap().into(),
                    ))
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                while !server_pid_file.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("python3 is required for Linux worker tests");
            if silent {
                // Keep the TCP connection open but do not read/respond to WebSocket pings.
                tokio::time::sleep(Duration::from_secs(10)).await;
            } else {
                let _ = socket.close(None).await;
            }
        });
        let began = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            run_session(
                Arc::new(config),
                reqwest::Client::new(),
                None,
                vec![Driver::Python],
                CancellationToken::new(),
            ),
        )
        .await
        .expect("lost server must stop the attempt promptly");
        assert!(result.is_err());
        if silent {
            assert!(began.elapsed() < Duration::from_secs(8));
        }
        let pid: u32 = tokio::fs::read_to_string(&pid_file)
            .await
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if !process::is_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a child process survived the lost session");
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn disconnect_stops_running_process_tree() {
        lost_session_kills_tree(false).await;
    }

    #[tokio::test]
    async fn unresponsive_server_stops_running_process_tree() {
        lost_session_kills_tree(true).await;
    }
}
