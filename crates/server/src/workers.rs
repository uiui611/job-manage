use crate::{
    ApiError, ApiResult, App, auth,
    db::{self, Run},
    routes,
};
use anyhow::{Result, bail};
use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use futures_util::SinkExt;
use job_protocol::{
    Assignment, DefinitionSpec, DownloadFile, Driver, InputFile, MAX_LOG_BYTES, MAX_RESULT_BYTES,
    Outcome, PROTOCOL_VERSION, ServerMessage, VERSION, WorkerMessage,
};
use serde_json::json;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub token_id: String,
    pub tx: mpsc::Sender<ServerMessage>,
    pub close: CancellationToken,
    pub tags: Vec<String>,
    pub drivers: Vec<Driver>,
    pub active: Option<(String, String)>,
}

pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let token = auth::bearer(&headers)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Worker token required".into()))?;
    let token_id = auth::check_token(&app, token, "worker")
        .await
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Invalid worker token".into()))?;
    Ok(ws
        .max_message_size(2 * 1024 * 1024)
        .max_frame_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| connection(app, socket, token_id)))
}
async fn send(socket: &mut WebSocket, message: ServerMessage) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(serde_json::to_string(&message)?.into())),
    )
    .await??;
    Ok(())
}
async fn connection(app: App, mut socket: WebSocket, token_id: String) {
    let hello = tokio::time::timeout(Duration::from_secs(10), socket.recv()).await;
    let parsed = match hello {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<WorkerMessage>(&text).ok(),
        _ => None,
    };
    let Some(WorkerMessage::Hello {
        worker_id,
        name,
        version,
        protocol_version,
        tags,
        drivers,
    }) = parsed
    else {
        let _ = send(
            &mut socket,
            ServerMessage::Error {
                code: "invalid_hello".into(),
                message: "First message must be hello".into(),
            },
        )
        .await;
        return;
    };
    if version != VERSION || protocol_version != PROTOCOL_VERSION {
        let _ = send(
            &mut socket,
            ServerMessage::Error {
                code: "version_mismatch".into(),
                message: format!(
                    "Server requires application {VERSION}, protocol {PROTOCOL_VERSION}"
                ),
            },
        )
        .await;
        return;
    }
    if !job_protocol::safe_file_name(&worker_id)
        || name.is_empty()
        || name.len() > 160
        || tags.len() > 64
        || tags.iter().any(|t| t.is_empty() || t.len() > 128)
        || drivers.is_empty()
        || drivers.len() > 2
    {
        let _ = send(
            &mut socket,
            ServerMessage::Error {
                code: "invalid_hello".into(),
                message: "Invalid worker identity or capabilities".into(),
            },
        )
        .await;
        return;
    }
    let session_id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel(64);
    let close = CancellationToken::new();
    let registered:Result<()>=crate::bounded(10, async {
        let _lock=app.dispatch_lock.lock().await;
        if let Some(old)=app.sessions.lock().await.remove(&worker_id) {
            old.close.cancel();db::fail_active(&app.pool,Some(&old.id),"worker_reconnected: previous connection was interrupted").await?;
        }
        sqlx::query("INSERT INTO workers (id,name,version,tags,drivers,status,session_id) VALUES ($1,$2,$3,$4,$5,'online',$6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,version=excluded.version,tags=excluded.tags,drivers=excluded.drivers,status='online',session_id=excluded.session_id,last_seen=now()")
            .bind(&worker_id).bind(name).bind(version).bind(json!(tags)).bind(json!(drivers)).bind(&session_id).execute(&app.pool).await?;
        app.sessions.lock().await.insert(worker_id.clone(),Session {id:session_id.clone(),token_id,tx,close:close.clone(),tags,drivers,active:None});Ok(())
    }).await;
    if let Err(error) = registered {
        tracing::warn!(%error,"worker registration failed");
        return;
    }
    let welcome = send(
        &mut socket,
        ServerMessage::Welcome {
            session_id: session_id.clone(),
            heartbeat_seconds: (app.config.heartbeat_timeout / 3).clamp(1, 10),
            heartbeat_timeout_seconds: app.config.heartbeat_timeout,
            version: VERSION.into(),
        },
    )
    .await;
    let mut last_heartbeat = Instant::now();
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    let mut logs: HashMap<String, LogBuffer> = HashMap::new();
    let operation_timeout = (app.config.heartbeat_timeout / 2).max(1);
    let reason = if welcome.is_err() {
        "worker_disconnected"
    } else {
        loop {
            tokio::select! {
                _=close.cancelled()=>break "worker_disconnected",
                _=timer.tick()=>{
                    if last_heartbeat.elapsed()>Duration::from_secs(app.config.heartbeat_timeout) {break "heartbeat_timeout";}
                    if let Err(error)=crate::bounded(operation_timeout.min(5),flush_logs(&app,&mut logs)).await {tracing::warn!(%error,"log persistence interrupted");break "database_unavailable";}
                }
                outbound=rx.recv()=>match outbound {
                    Some(message)=>if send(&mut socket,message).await.is_err(){break "worker_disconnected";},
                    None=>break "worker_disconnected",
                },
                inbound=socket.recv()=>{
                    match inbound {
                        Some(Ok(Message::Text(text)))=>{
                            let message=match serde_json::from_str::<WorkerMessage>(&text) {Ok(m)=>m,Err(_)=>break "invalid_worker_message"};
                            if matches!(message,WorkerMessage::Heartbeat{}) {last_heartbeat=Instant::now();}
                            match crate::bounded(operation_timeout,handle(&app,&worker_id,&session_id,message,&mut logs)).await {
                                Ok(Some(reply))=>if send(&mut socket,reply).await.is_err(){break "worker_disconnected";},
                                Ok(None)=>{},Err(error)=>{tracing::warn!(%error,worker=%worker_id,"worker message failed");break "worker_protocol_error";}
                            }
                        }
                        Some(Ok(Message::Ping(_)))|Some(Ok(Message::Pong(_)))=>{},
                        Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break "worker_disconnected",
                        _=>break "invalid_worker_message",
                    }
                }
            }
        }
    };
    close.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
    // Remove the session before waiting for the DB so it can never receive more assignments.
    {
        let _lock = app.dispatch_lock.lock().await;
        let mut sessions = app.sessions.lock().await;
        if sessions.get(&worker_id).is_some_and(|s| s.id == session_id) {
            sessions.remove(&worker_id);
        }
    }
    // Retain cleanup through transient DB failures. The next startup also fails active attempts.
    loop {
        let result: Result<()> = crate::bounded(10, async {
            let _lock = app.dispatch_lock.lock().await;
            db::fail_active(&app.pool, Some(&session_id), reason).await?;
            sqlx::query(
                "UPDATE workers SET status='offline',session_id=NULL WHERE id=$1 AND session_id=$2",
            )
            .bind(&worker_id)
            .bind(&session_id)
            .execute(&app.pool)
            .await?;
            Ok(())
        })
        .await;
        if result.is_ok() {
            break;
        }
        tracing::warn!(worker=%worker_id,"retrying disconnected worker status persistence");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let _ = crate::bounded(5, flush_logs(&app, &mut logs)).await;
    tracing::info!(worker=%worker_id,%reason,"worker disconnected; active runs failed without retry");
}

struct LogBuffer {
    run_id: String,
    pending: String,
    total: usize,
    truncated: bool,
    flushed_truncation: bool,
}
fn limited_text(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
async fn flush_logs(app: &App, logs: &mut HashMap<String, LogBuffer>) -> Result<()> {
    for log in logs.values_mut() {
        if log.pending.is_empty() && (!log.truncated || log.flushed_truncation) {
            continue;
        }
        sqlx::query("INSERT INTO run_logs (run_id,text,truncated) VALUES ($1,$2,$3) ON CONFLICT(run_id) DO UPDATE SET text=run_logs.text || excluded.text,truncated=run_logs.truncated OR excluded.truncated")
            .bind(&log.run_id).bind(&log.pending).bind(log.truncated).execute(&app.pool).await?;
        log.pending.clear();
        log.flushed_truncation = log.truncated;
    }
    Ok(())
}
async fn active(app: &App, worker: &str, session: &str, attempt: &str) -> Result<Option<Run>> {
    Ok(sqlx::query_as("SELECT * FROM runs WHERE worker_id=$1 AND session_id=$2 AND attempt_id=$3 AND status IN ('assigned','running')")
        .bind(worker).bind(session).bind(attempt).fetch_optional(&app.pool).await?)
}
async fn handle(
    app: &App,
    worker: &str,
    session: &str,
    message: WorkerMessage,
    logs: &mut HashMap<String, LogBuffer>,
) -> Result<Option<ServerMessage>> {
    if let WorkerMessage::LogTruncated { attempt_id } = &message {
        let run = app
            .sessions
            .lock()
            .await
            .get(worker)
            .filter(|s| s.id == session)
            .and_then(|s| s.active.as_ref())
            .filter(|(_, a)| a == attempt_id)
            .map(|(r, _)| r.clone());
        if let Some(run_id) = run {
            logs.entry(attempt_id.clone())
                .or_insert_with(|| LogBuffer {
                    run_id,
                    pending: String::new(),
                    total: 0,
                    truncated: false,
                    flushed_truncation: false,
                })
                .truncated = true;
        }
        return Ok(None);
    }
    if let WorkerMessage::Log {
        attempt_id,
        stream,
        text,
    } = &message
    {
        if stream != "stdout" && stream != "stderr" {
            bail!("Invalid log stream");
        }
        let run = app
            .sessions
            .lock()
            .await
            .get(worker)
            .filter(|s| s.id == session)
            .and_then(|s| s.active.as_ref())
            .filter(|(_, a)| a == attempt_id)
            .map(|(r, _)| r.clone());
        if let Some(run_id) = run {
            let log = logs.entry(attempt_id.clone()).or_insert_with(|| LogBuffer {
                run_id,
                pending: String::new(),
                total: 0,
                truncated: false,
                flushed_truncation: false,
            });
            let chunk = limited_text(text, MAX_LOG_BYTES.saturating_sub(log.total));
            log.pending.push_str(chunk);
            log.total += chunk.len();
            if chunk.len() < text.len() {
                log.truncated = true;
            }
        }
        return Ok(None);
    }
    if matches!(message, WorkerMessage::Heartbeat {}) {
        sqlx::query("UPDATE workers SET last_seen=now() WHERE id=$1 AND session_id=$2")
            .bind(worker)
            .bind(session)
            .execute(&app.pool)
            .await?;
        return Ok(None);
    }
    match message {
        WorkerMessage::Started { attempt_id } => {
            let _lock = app.dispatch_lock.lock().await;
            sqlx::query("UPDATE runs SET status='running' WHERE attempt_id=$1 AND session_id=$2 AND status='assigned'").bind(attempt_id).bind(session).execute(&app.pool).await?;
            Ok(None)
        }
        WorkerMessage::RequestUpload {
            request_id,
            attempt_id,
            name,
            size,
        } => {
            let _lock = app.dispatch_lock.lock().await;
            let run = active(app, worker, session, &attempt_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Attempt is no longer active"))?;
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE attempt_id=$1")
                    .bind(&attempt_id)
                    .fetch_one(&app.pool)
                    .await?;
            if count >= 32 {
                bail!("At most 32 artifacts per attempt");
            }
            let artifact =
                routes::make_artifact(app, &name, size, Some(&run.id), Some(&attempt_id))
                    .await
                    .map_err(|e| anyhow::anyhow!(e.1))?;
            let url = app.storage.url(&artifact, Method::PUT).await?;
            Ok(Some(ServerMessage::UploadUrl {
                request_id,
                artifact_id: artifact.id,
                url,
            }))
        }
        WorkerMessage::Finished {
            attempt_id,
            outcome,
            exit_code,
            result,
            error,
            artifact_ids,
        } => {
            let Some(run) = active(app, worker, session, &attempt_id).await? else {
                return Ok(Some(ServerMessage::FinishedAck {
                    attempt_id,
                    accepted: false,
                }));
            };
            flush_logs(app, logs).await?;
            if result
                .as_ref()
                .is_some_and(|r| r.to_string().len() > MAX_RESULT_BYTES)
                || artifact_ids.len() > 32
            {
                bail!("Result exceeds limits");
            }
            // Network storage verification must not hold the global scheduling lock.
            // Bound the entire operation below the heartbeat deadline, including metadata reads.
            let verification = tokio::time::timeout(
                Duration::from_secs((app.config.heartbeat_timeout / 3).clamp(1, 10)),
                async {
                    use futures_util::StreamExt;
                    let mut checked = Vec::new();
                    for batch in artifact_ids.chunks(4) {
                        let mut checks = futures_util::stream::FuturesUnordered::new();
                        for id in batch {
                            checks.push(verify_artifact(
                                app.clone(),
                                id.clone(),
                                attempt_id.clone(),
                                run.id.clone(),
                            ));
                        }
                        while let Some(check) = checks.next().await {
                            checked.push(check?);
                        }
                    }
                    Ok::<_, anyhow::Error>(checked)
                },
            )
            .await;
            let (checked, mut verification_error) = match verification {
                Ok(Ok(checked)) => (checked, None),
                _ => (
                    Vec::new(),
                    Some("artifact_verification_failed_or_timed_out".to_string()),
                ),
            };
            if outcome == Outcome::Succeeded && exit_code != Some(0) {
                verification_error = Some("invalid_success_exit_code".into());
            }
            let _lock = app.dispatch_lock.lock().await;
            let status = if verification_error.is_some() {
                "failed"
            } else {
                match outcome {
                    Outcome::Succeeded => "succeeded",
                    Outcome::Failed => "failed",
                    Outcome::Canceled => "canceled",
                }
            };
            let final_error =
                verification_error.or(error.map(|s| limited_text(&s, 4096).to_string()));
            let mut transaction = app.pool.begin().await?;
            let changed=sqlx::query("UPDATE runs SET status=$3,result=$4,error=$5,exit_code=$6,finished_at=now() WHERE attempt_id=$1 AND session_id=$2 AND status IN ('assigned','running')")
                .bind(&attempt_id).bind(session).bind(status).bind(result).bind(final_error).bind(exit_code).execute(&mut *transaction).await?.rows_affected();
            if changed > 0 {
                for id in checked {
                    sqlx::query("UPDATE artifacts SET status='complete' WHERE id=$1")
                        .bind(id)
                        .execute(&mut *transaction)
                        .await?;
                }
                sqlx::query("UPDATE workers SET status='online' WHERE id=$1 AND session_id=$2")
                    .bind(worker)
                    .bind(session)
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            logs.remove(&attempt_id);
            if let Some(s) = app
                .sessions
                .lock()
                .await
                .get_mut(worker)
                .filter(|s| s.id == session)
            {
                s.active = None;
            }
            Ok(Some(ServerMessage::FinishedAck {
                attempt_id,
                accepted: changed > 0,
            }))
        }
        _ => bail!("Unexpected worker message"),
    }
}

async fn verify_artifact(app: App, id: String, attempt: String, run_id: String) -> Result<String> {
    let artifact = routes::get_artifact(&app, &id)
        .await
        .map_err(|e| anyhow::anyhow!(e.1))?;
    if artifact.attempt_id.as_deref() != Some(&attempt)
        || artifact.run_id.as_deref() != Some(&run_id)
    {
        bail!("Artifact belongs to another attempt");
    }
    app.storage.verify_size(&artifact).await?;
    Ok(id)
}

pub async fn dispatch(app: &App) -> Result<()> {
    if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let _lock = app.dispatch_lock.lock().await;
    // Server watchdog backs up worker timeouts and stops an agent that never acknowledges assignment.
    let overdue:Vec<Run>=sqlx::query_as("SELECT * FROM runs WHERE (status='assigned' AND started_at < now() - INTERVAL '30 seconds') OR (status='running' AND started_at < now() - (CAST(spec->>'timeout_seconds' AS BIGINT)+60)*INTERVAL '1 second') OR (status IN ('assigned','running') AND cancel_requested_at < now() - INTERVAL '10 seconds')")
        .fetch_all(&app.pool).await?;
    for run in overdue {
        if let Some(session) = run.session_id.as_deref() {
            db::fail_active(&app.pool, Some(session), "execution_deadline_exceeded").await?;
            for s in app.sessions.lock().await.values() {
                if s.id == session {
                    s.close.cancel();
                }
            }
        }
    }
    if routes::paused(app).await? {
        return Ok(());
    }
    let sessions: Vec<_> = app
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, s)| (id.clone(), s.clone()))
        .collect();
    for (worker, session) in sessions {
        if session.close.is_cancelled() || session.active.is_some() {
            continue;
        }
        let enabled: bool =
            sqlx::query_scalar("SELECT enabled FROM workers WHERE id=$1 AND session_id=$2 AND NOT EXISTS (SELECT 1 FROM runs WHERE worker_id=$1 AND session_id=$2 AND status IN ('assigned','running'))")
                .bind(&worker)
                .bind(&session.id)
                .fetch_optional(&app.pool)
                .await?
                .unwrap_or(false);
        if !enabled {
            continue;
        }
        let drivers: Vec<&str> = session
            .drivers
            .iter()
            .map(|d| match d {
                Driver::Node => "node",
                Driver::Python => "python",
            })
            .collect();
        let candidate:Option<Run>=sqlx::query_as("SELECT * FROM runs WHERE status='queued' AND COALESCE(spec->'required_tags','[]'::JSONB) <@ $1::JSONB AND spec->>'driver'=ANY($2) ORDER BY created_at,id LIMIT 1")
            .bind(json!(session.tags)).bind(drivers).fetch_optional(&app.pool).await?;
        let Some(run) = candidate else {
            continue;
        };
        let definition: DefinitionSpec = serde_json::from_value(run.spec)?;
        let inputs: Vec<InputFile> = serde_json::from_value(run.files)?;
        let mut files = Vec::new();
        let mut input_error = false;
        for input in inputs {
            match routes::get_artifact(app, &input.artifact_id).await {
                Ok(artifact) if artifact.status == "complete" => {
                    files.push(DownloadFile {
                        name: input.name,
                        url: app.storage.url(&artifact, Method::GET).await?,
                        size: artifact.size as u64,
                    });
                }
                _ => {
                    input_error = true;
                    break;
                }
            }
        }
        if input_error {
            sqlx::query("UPDATE runs SET status='failed',error='input_artifact_unavailable',finished_at=now() WHERE id=$1 AND status='queued'").bind(&run.id).execute(&app.pool).await?;
            continue;
        }
        let attempt = Uuid::new_v4().to_string();
        let mut transaction = app.pool.begin().await?;
        let changed=sqlx::query("UPDATE runs SET status='assigned',worker_id=$2,session_id=$3,attempt_id=$4,started_at=now() WHERE id=$1 AND status='queued'")
            .bind(&run.id).bind(&worker).bind(&session.id).bind(&attempt).execute(&mut *transaction).await?.rows_affected();
        if changed == 0 {
            continue;
        }
        sqlx::query("UPDATE workers SET status='busy' WHERE id=$1 AND session_id=$2")
            .bind(&worker)
            .bind(&session.id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        if let Some(s) = app
            .sessions
            .lock()
            .await
            .get_mut(&worker)
            .filter(|s| s.id == session.id)
        {
            s.active = Some((run.id.clone(), attempt.clone()));
        }
        if session
            .tx
            .try_send(ServerMessage::Assign {
                assignment: Assignment {
                    run_id: run.id,
                    attempt_id: attempt,
                    definition,
                    input: run.input,
                    files,
                },
            })
            .is_err()
        {
            session.close.cancel();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_limit_preserves_utf8() {
        assert_eq!(limited_text("あいう", 4), "あ");
        assert_eq!(limited_text("abc", 2), "ab");
    }
}
