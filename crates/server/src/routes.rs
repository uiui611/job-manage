use crate::{
    ApiError, ApiResult, App, auth,
    db::{Artifact, Definition, Run, Worker},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use job_protocol::{DefinitionSpec, Driver, InputFile, safe_file_name};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<App> {
    Router::new()
        .route(
            "/openapi.json",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    include_str!("../../../docs/openapi.json"),
                )
            }),
        )
        .route(
            "/definitions",
            get(list_definitions).post(create_definition),
        )
        .route(
            "/definitions/{id}",
            get(get_definition).put(update_definition),
        )
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/retry", post(retry_run))
        .route("/runs/{id}/logs", get(run_logs))
        .route("/runs/{id}/artifacts", get(run_artifacts))
        .route("/workers", get(list_workers))
        .route("/workers/{id}", axum::routing::patch(update_worker))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/settings", get(settings).patch(update_settings))
        .route("/artifacts/uploads", post(create_upload))
        .route("/artifacts/{id}/complete", post(complete_upload))
}

fn validate_spec(spec: &DefinitionSpec) -> ApiResult<()> {
    if spec.name.trim().is_empty() || spec.name.len() > 160 {
        return Err(ApiError::bad("Name must be 1..160 bytes"));
    }
    if spec.description.len() > 4096 || spec.script.is_empty() || spec.script.len() > 256 * 1024 {
        return Err(ApiError::bad(
            "Script must be 1..262144 bytes and description at most 4096 bytes",
        ));
    }
    if !(1..=604800).contains(&spec.timeout_seconds) {
        return Err(ApiError::bad("timeout_seconds must be 1..604800"));
    }
    if spec.required_tags.len() > 64
        || spec
            .required_tags
            .iter()
            .any(|t| t.is_empty() || t.len() > 128)
    {
        return Err(ApiError::bad("Invalid required_tags"));
    }
    if spec.input_schema.to_string().len() > 32768 || external_ref(&spec.input_schema) {
        return Err(ApiError::bad(
            "Schema must be <=32KiB and may only use local # references",
        ));
    }
    jsonschema::validator_for(&spec.input_schema)
        .map_err(|e| ApiError::bad(format!("Invalid JSON Schema: {e}")))?;
    Ok(())
}
fn external_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| {
            ((k == "$ref" || k == "$dynamicRef" || k == "$recursiveRef")
                && !v.as_str().is_some_and(|s| s.starts_with('#')))
                || external_ref(v)
        }),
        Value::Array(a) => a.iter().any(external_ref),
        _ => false,
    }
}
async fn list_definitions(State(app): State<App>) -> ApiResult<Json<Vec<Definition>>> {
    Ok(Json(
        sqlx::query_as("SELECT * FROM definitions ORDER BY updated_at DESC LIMIT 1000")
            .fetch_all(&app.pool)
            .await?,
    ))
}
async fn get_definition(
    State(app): State<App>,
    Path(id): Path<String>,
) -> ApiResult<Json<Definition>> {
    Ok(Json(
        sqlx::query_as("SELECT * FROM definitions WHERE id=$1")
            .bind(id)
            .fetch_optional(&app.pool)
            .await?
            .ok_or_else(ApiError::missing)?,
    ))
}
async fn create_definition(
    State(app): State<App>,
    Json(spec): Json<DefinitionSpec>,
) -> ApiResult<(StatusCode, Json<Definition>)> {
    validate_spec(&spec)?;
    let row =
        sqlx::query_as("INSERT INTO definitions (id,revision,spec) VALUES ($1,1,$2) RETURNING *")
            .bind(Uuid::new_v4().to_string())
            .bind(json!(spec))
            .fetch_one(&app.pool)
            .await?;
    Ok((StatusCode::CREATED, Json(row)))
}
async fn update_definition(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(spec): Json<DefinitionSpec>,
) -> ApiResult<Json<Definition>> {
    validate_spec(&spec)?;
    Ok(Json(sqlx::query_as("UPDATE definitions SET revision=revision+1,spec=$2,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(id).bind(json!(spec)).fetch_optional(&app.pool).await?.ok_or_else(ApiError::missing)?))
}

#[derive(Deserialize, serde::Serialize)]
struct Submit {
    definition_id: String,
    #[serde(default = "empty_object")]
    input: Value,
    #[serde(default)]
    files: Vec<InputFile>,
}
fn empty_object() -> Value {
    json!({})
}
async fn validate_files(app: &App, files: &[InputFile]) -> ApiResult<()> {
    if files.len() > 32 {
        return Err(ApiError::bad("At most 32 input files"));
    }
    let mut names = std::collections::HashSet::new();
    let mut total_size = 0_i64;
    for file in files {
        if !safe_file_name(&file.name) || !names.insert(file.name.to_ascii_lowercase()) {
            return Err(ApiError::bad(
                "Input filenames must be unique safe ASCII filenames",
            ));
        }
        let size: Option<i64> =
            sqlx::query_scalar("SELECT size FROM artifacts WHERE id=$1 AND status='complete'")
                .bind(&file.artifact_id)
                .fetch_optional(&app.pool)
                .await?;
        let Some(size) = size else {
            return Err(ApiError::bad(format!(
                "Input artifact {} is not complete",
                file.artifact_id
            )));
        };
        total_size += size;
        if total_size > 512 * 1024 * 1024 {
            return Err(ApiError::bad("Total input files must not exceed 512 MiB"));
        }
    }
    Ok(())
}
async fn create_run(
    State(app): State<App>,
    headers: HeaderMap,
    Json(submit): Json<Submit>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let key = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 128) {
        return Err(ApiError::bad("Idempotency-Key must be 1..128 bytes"));
    }
    let request_hash = auth::hash(&json!(submit).to_string());
    if let Some(key) = &key
        && let Some(existing) =
            sqlx::query_as::<_, Run>("SELECT * FROM runs WHERE idempotency_key=$1")
                .bind(key)
                .fetch_optional(&app.pool)
                .await?
    {
        if existing.request_hash.as_deref() != Some(&request_hash) {
            return Err(ApiError::conflict(
                "Idempotency-Key was already used with different input",
            ));
        }
        return Ok((StatusCode::OK, Json(json!(existing))));
    }
    let definition: Definition = sqlx::query_as("SELECT * FROM definitions WHERE id=$1")
        .bind(&submit.definition_id)
        .fetch_optional(&app.pool)
        .await?
        .ok_or_else(ApiError::missing)?;
    let spec: DefinitionSpec = serde_json::from_value(definition.spec.clone())
        .map_err(|_| ApiError::bad("Stored definition is invalid"))?;
    if submit.input.to_string().len() > job_protocol::MAX_RESULT_BYTES {
        return Err(ApiError::bad("Input exceeds 1 MiB"));
    }
    let validator =
        jsonschema::validator_for(&spec.input_schema).map_err(|e| ApiError::bad(e.to_string()))?;
    if let Err(error) = validator.validate(&submit.input) {
        return Err(ApiError::bad(format!("Input validation failed: {error}")));
    }
    validate_files(&app, &submit.files).await?;
    let row = sqlx::query_as::<_, Run>("INSERT INTO runs (id,definition_id,definition_name,revision,spec,input,files,status,idempotency_key,request_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,'queued',$8,$9) ON CONFLICT(idempotency_key) DO NOTHING RETURNING *")
        .bind(Uuid::new_v4().to_string()).bind(&definition.id).bind(&spec.name).bind(definition.revision).bind(definition.spec)
        .bind(submit.input).bind(json!(submit.files)).bind(&key).bind(&request_hash).fetch_optional(&app.pool).await?;
    let run = if let Some(row) = row {
        row
    } else {
        let existing: Run = sqlx::query_as("SELECT * FROM runs WHERE idempotency_key=$1")
            .bind(key)
            .fetch_one(&app.pool)
            .await?;
        if existing.request_hash.as_deref() != Some(&request_hash) {
            return Err(ApiError::conflict(
                "Idempotency-Key was already used with different input",
            ));
        }
        existing
    };
    Ok((StatusCode::ACCEPTED, Json(json!(run))))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}
async fn list_runs(
    State(app): State<App>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let rows: Vec<Run> = sqlx::query_as("SELECT * FROM runs WHERE ($1::TEXT IS NULL OR status=$1) ORDER BY created_at DESC LIMIT $2")
        .bind(query.status).bind(query.limit.unwrap_or(100).clamp(1, 500)).fetch_all(&app.pool).await?;
    let workers: Vec<Worker> = sqlx::query_as("SELECT * FROM workers")
        .fetch_all(&app.pool)
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|run| decorate_run(run, &workers))
            .collect(),
    ))
}
pub async fn fetch_run(app: &App, id: &str) -> ApiResult<Run> {
    sqlx::query_as("SELECT * FROM runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&app.pool)
        .await?
        .ok_or_else(ApiError::missing)
}
fn decorate_run(run: Run, workers: &[Worker]) -> Value {
    let reason = if run.status == "queued" {
        let spec: Option<DefinitionSpec> = serde_json::from_value(run.spec.clone()).ok();
        let matches: Vec<_> = workers
            .iter()
            .filter(|w| {
                spec.as_ref().is_some_and(|s| {
                    let tags: Vec<String> =
                        serde_json::from_value(w.tags.clone()).unwrap_or_default();
                    let drivers: Vec<Driver> =
                        serde_json::from_value(w.drivers.clone()).unwrap_or_default();
                    job_protocol::tags_match(&s.required_tags, &tags) && drivers.contains(&s.driver)
                })
            })
            .collect();
        Some(if matches.is_empty() {
            "no_matching_worker"
        } else if matches.iter().all(|w| !w.enabled) {
            "workers_disabled"
        } else if matches.iter().all(|w| w.status == "offline") {
            "workers_offline"
        } else {
            "waiting_for_slot"
        })
    } else {
        None
    };
    let mut value = json!(run);
    value["waiting_reason"] = json!(reason);
    value
}
async fn get_run(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let run = fetch_run(&app, &id).await?;
    let workers = sqlx::query_as("SELECT * FROM workers")
        .fetch_all(&app.pool)
        .await?;
    Ok(Json(decorate_run(run, &workers)))
}
async fn cancel_run(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Json<Run>> {
    // Serializes with assignment, disconnect, and completion. Terminal states cannot be overwritten.
    let _dispatch = app.dispatch_lock.lock().await;
    let run = fetch_run(&app, &id).await?;
    if run.status == "queued" {
        sqlx::query("UPDATE runs SET status='canceled',cancel_requested=TRUE,finished_at=now() WHERE id=$1 AND status='queued'").bind(&id).execute(&app.pool).await?;
    } else if run.status == "assigned" || run.status == "running" {
        sqlx::query("UPDATE runs SET cancel_requested=TRUE,cancel_requested_at=COALESCE(cancel_requested_at,now()) WHERE id=$1 AND status IN ('assigned','running')").bind(&id).execute(&app.pool).await?;
        if let (Some(worker), Some(attempt)) = (&run.worker_id, &run.attempt_id)
            && let Some(session) = app.sessions.lock().await.get(worker)
            && session
                .tx
                .try_send(job_protocol::ServerMessage::Cancel {
                    attempt_id: attempt.clone(),
                })
                .is_err()
        {
            session.close.cancel();
        }
    }
    Ok(Json(fetch_run(&app, &id).await?))
}
async fn retry_run(
    State(app): State<App>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    let old = fetch_run(&app, &id).await?;
    if !["succeeded", "failed", "canceled"].contains(&old.status.as_str()) {
        return Err(ApiError::conflict(
            "Only terminal runs can be manually repeated",
        ));
    }
    let row = sqlx::query_as("INSERT INTO runs (id,definition_id,definition_name,revision,spec,input,files,status) VALUES ($1,$2,$3,$4,$5,$6,$7,'queued') RETURNING *")
        .bind(Uuid::new_v4().to_string()).bind(old.definition_id).bind(old.definition_name).bind(old.revision).bind(old.spec).bind(old.input).bind(old.files).fetch_one(&app.pool).await?;
    Ok((StatusCode::ACCEPTED, Json(row)))
}
async fn run_logs(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    fetch_run(&app, &id).await?;
    let row = sqlx::query("SELECT text,truncated FROM run_logs WHERE run_id=$1")
        .bind(id)
        .fetch_optional(&app.pool)
        .await?;
    Ok(Json(match row {
        Some(r) => {
            json!({"text":r.get::<String,_>("text"),"truncated":r.get::<bool,_>("truncated")})
        }
        None => json!({"text":"","truncated":false}),
    }))
}
async fn run_artifacts(
    State(app): State<App>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Value>>> {
    fetch_run(&app, &id).await?;
    let artifacts: Vec<Artifact> = sqlx::query_as(
        "SELECT * FROM artifacts WHERE run_id=$1 AND status='complete' ORDER BY name",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await?;
    let mut results = Vec::new();
    for artifact in artifacts {
        let url = app.storage.url(&artifact, Method::GET).await?;
        let mut value = json!(artifact);
        value["url"] = json!(url);
        results.push(value);
    }
    Ok(Json(results))
}
async fn list_workers(State(app): State<App>) -> ApiResult<Json<Vec<Value>>> {
    let rows: Vec<Worker> = sqlx::query_as("SELECT * FROM workers ORDER BY name")
        .fetch_all(&app.pool)
        .await?;
    let active: Vec<(String,String)> = sqlx::query_as("SELECT worker_id,id FROM runs WHERE status IN ('assigned','running') AND worker_id IS NOT NULL").fetch_all(&app.pool).await?;
    Ok(Json(
        rows.into_iter()
            .map(|w| {
                let run = active
                    .iter()
                    .find(|(id, _)| id == &w.id)
                    .map(|(_, r)| r.clone());
                let mut value = json!(w);
                value["active_run_id"] = json!(run);
                value
            })
            .collect(),
    ))
}
#[derive(Deserialize)]
struct WorkerUpdate {
    enabled: bool,
}
async fn update_worker(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<WorkerUpdate>,
) -> ApiResult<Json<Value>> {
    let affected = sqlx::query("UPDATE workers SET enabled=$2 WHERE id=$1")
        .bind(id)
        .bind(body.enabled)
        .execute(&app.pool)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(ApiError::missing());
    }
    Ok(Json(json!({"enabled":body.enabled})))
}
async fn list_tokens(State(app): State<App>) -> ApiResult<Json<Vec<Value>>> {
    let rows =
        sqlx::query("SELECT id,name,kind,revoked,created_at FROM tokens ORDER BY created_at DESC")
            .fetch_all(&app.pool)
            .await?;
    Ok(Json(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),"revoked":r.get::<bool,_>("revoked"),"created_at":r.get::<chrono::DateTime<Utc>,_>("created_at")})).collect()))
}
#[derive(Deserialize)]
struct TokenRequest {
    name: String,
    kind: String,
}
async fn create_token(
    State(app): State<App>,
    Json(body): Json<TokenRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if body.name.trim().is_empty()
        || body.name.len() > 160
        || !["api", "worker"].contains(&body.kind.as_str())
    {
        return Err(ApiError::bad(
            "Token name and kind(api/worker) are required",
        ));
    }
    let id = Uuid::new_v4().to_string();
    let token = format!("jp_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO tokens (id,name,kind,token_hash) VALUES ($1,$2,$3,$4)")
        .bind(&id)
        .bind(body.name)
        .bind(body.kind)
        .bind(auth::hash(&token))
        .execute(&app.pool)
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"id":id,"token":token}))))
}
async fn revoke_token(State(app): State<App>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if sqlx::query("UPDATE tokens SET revoked=TRUE WHERE id=$1")
        .bind(&id)
        .execute(&app.pool)
        .await?
        .rows_affected()
        == 0
    {
        return Err(ApiError::missing());
    }
    for session in app.sessions.lock().await.values() {
        if session.token_id == id {
            session.close.cancel();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
pub async fn paused(app: &App) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Value>(
        "SELECT value FROM platform_settings WHERE key='scheduler_paused'",
    )
    .fetch_optional(&app.pool)
    .await?
    .and_then(|v| v.as_bool())
    .unwrap_or(false))
}
async fn settings(State(app): State<App>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"version":job_protocol::VERSION,"base_path":app.config.base_path,"heartbeat_timeout_seconds":app.config.heartbeat_timeout,"max_artifact_bytes":app.config.max_artifact_bytes,"max_total_file_bytes":512*1024*1024,"max_files":32,"storage":app.storage.kind(),"auto_retry":false,"scheduler_paused":paused(&app).await?}),
    ))
}
#[derive(Deserialize)]
struct SettingsUpdate {
    scheduler_paused: bool,
}
async fn update_settings(
    State(app): State<App>,
    Json(body): Json<SettingsUpdate>,
) -> ApiResult<Json<Value>> {
    sqlx::query("INSERT INTO platform_settings (key,value) VALUES ('scheduler_paused',$1) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(json!(body.scheduler_paused)).execute(&app.pool).await?;
    settings(State(app)).await
}

#[derive(Deserialize)]
struct UploadRequest {
    name: String,
    size: u64,
}
pub async fn make_artifact(
    app: &App,
    name: &str,
    size: u64,
    run: Option<&str>,
    attempt: Option<&str>,
) -> ApiResult<Artifact> {
    if !safe_file_name(name) || size > app.config.max_artifact_bytes {
        return Err(ApiError::bad(
            "Invalid artifact filename or size exceeds 64 MiB",
        ));
    }
    let id = Uuid::new_v4().to_string();
    let key = format!("artifacts/{id}/{name}");
    Ok(sqlx::query_as("INSERT INTO artifacts (id,run_id,attempt_id,name,size,object_key) VALUES ($1,$2,$3,$4,$5,$6) RETURNING *")
        .bind(id).bind(run).bind(attempt).bind(name).bind(size as i64).bind(key).fetch_one(&app.pool).await?)
}
async fn create_upload(
    State(app): State<App>,
    Json(body): Json<UploadRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let artifact = make_artifact(&app, &body.name, body.size, None, None).await?;
    let url = app.storage.url(&artifact, Method::PUT).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id":artifact.id,"url":url})),
    ))
}
async fn complete_upload(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let artifact = get_artifact(&app, &id).await?;
    if artifact.run_id.is_some() {
        return Err(ApiError::bad(
            "Worker artifacts are finalized with their run",
        ));
    }
    app.storage.verify_size(&artifact).await?;
    sqlx::query("UPDATE artifacts SET status='complete' WHERE id=$1")
        .bind(id)
        .execute(&app.pool)
        .await?;
    Ok(Json(json!({"id":artifact.id,"status":"complete"})))
}
pub async fn get_artifact(app: &App, id: &str) -> ApiResult<Artifact> {
    sqlx::query_as("SELECT * FROM artifacts WHERE id=$1")
        .bind(id)
        .fetch_optional(&app.pool)
        .await?
        .ok_or_else(ApiError::missing)
}
#[derive(Deserialize)]
pub struct SignedQuery {
    expires: i64,
    signature: String,
}
pub async fn put_blob(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<SignedQuery>,
    body: bytes::Bytes,
) -> ApiResult<StatusCode> {
    if !app
        .storage
        .verify("PUT", &id, query.expires, &query.signature)
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Invalid or expired upload URL".into(),
        ));
    }
    let artifact = get_artifact(&app, &id).await?;
    if artifact.status != "pending" {
        return Err(ApiError::conflict("Artifact is already complete"));
    }
    app.storage.put_local(&artifact, body).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub async fn get_blob(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<SignedQuery>,
) -> ApiResult<Response> {
    if !app
        .storage
        .verify("GET", &id, query.expires, &query.signature)
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Invalid or expired download URL".into(),
        ));
    }
    if app.storage.s3.is_some() {
        return Err(ApiError::missing());
    }
    let artifact = get_artifact(&app, &id).await?;
    if artifact.status != "complete" {
        return Err(ApiError::missing());
    }
    let file = tokio::fs::File::open(app.storage.local.join(&id))
        .await
        .map_err(|_| ApiError::missing())?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", artifact.name),
            ),
            (header::CACHE_CONTROL, "private, no-store".into()),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_remote_schema_references() {
        assert!(external_ref(
            &json!({"properties":{"x":{"$ref":"https://example.invalid/schema"}}})
        ));
        assert!(!external_ref(&json!({"$ref":"#/$defs/thing"})));
    }
}
