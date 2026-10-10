mod auth;
mod config;
mod db;
mod routes;
mod storage;
mod workers;

use anyhow::Result;
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Redirect},
    routing::get,
};
use config::Config;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

#[derive(Clone)]
pub struct App(Arc<Inner>);
pub struct Inner {
    pub config: Config,
    pub pool: sqlx::PgPool,
    pub storage: storage::Storage,
    pub sessions: Mutex<HashMap<String, workers::Session>>,
    pub dispatch_lock: Mutex<()>,
    pub shutting_down: std::sync::atomic::AtomicBool,
}
impl std::ops::Deref for App {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);
impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    fn missing() -> Self {
        Self(StatusCode::NOT_FOUND, "Not found".into())
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, message.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, axum::Json(json!({"error":self.1}))).into_response()
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database operation failed");
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "Database temporarily unavailable".into(),
        )
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::warn!(%error, "operation failed");
        Self(
            StatusCode::BAD_REQUEST,
            "Storage or validation operation failed".into(),
        )
    }
}
pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub async fn bounded<T>(
    seconds: u64,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(seconds), operation)
        .await
        .map_err(|_| anyhow::anyhow!("Operation timed out"))?
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "job_server=info,tower_http=warn".into()),
        )
        .init();
    let config = Config::from_env()?;
    if config.auth_mode == "none" {
        tracing::warn!("Authentication disabled explicitly: development use only");
    }
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await?;
    db::migrate(&pool).await?;
    // A server restart severs every worker session. Never requeue these jobs.
    db::fail_active(
        &pool,
        None,
        "server_restarted: worker connection was interrupted",
    )
    .await?;
    sqlx::query("UPDATE workers SET status='offline', session_id=NULL")
        .execute(&pool)
        .await?;
    let storage = storage::Storage::new(&config).await?;
    let app = App(Arc::new(Inner {
        config: config.clone(),
        pool,
        storage,
        sessions: Mutex::new(HashMap::new()),
        dispatch_lock: Mutex::new(()),
        shutting_down: std::sync::atomic::AtomicBool::new(false),
    }));
    let api =
        routes::router().route_layer(middleware::from_fn_with_state(app.clone(), auth::api_auth));
    let mounted = web_files(&config.web_dir)
        .nest("/api/v1", api)
        .route(
            "/api/v1/blobs/{id}",
            get(routes::get_blob)
                .put(routes::put_blob)
                .layer(DefaultBodyLimit::max(config.max_artifact_bytes as usize)),
        )
        .route("/worker", get(workers::upgrade))
        .route("/config.json", get(configuration));
    let router = mount_base_path(mounted, &config.base_path);
    let router = router
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(app.clone());
    let background = app.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_millis(750));
        loop {
            timer.tick().await;
            if let Err(error) = bounded(15, workers::dispatch(&background)).await {
                tracing::warn!(%error, "dispatch deferred");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(bind=%config.bind, base_path=%config.base_path, version=job_protocol::VERSION, "JOB server listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown(app))
        .await?;
    Ok(())
}
fn web_files<S: Clone + Send + Sync + 'static>(directory: &Path) -> Router<S> {
    Router::new()
        // An explicit root route also matches the empty suffix of an Axum nest.
        .route_service("/", ServeFile::new(directory.join("index.html")))
        .fallback_service(ServeDir::new(directory).append_index_html_on_directories(true))
}
fn mount_base_path<S: Clone + Send + Sync + 'static>(mounted: Router<S>, base: &str) -> Router<S> {
    if base.is_empty() {
        mounted
    } else {
        let root = format!("{base}/");
        Router::new()
            .route(base, get(move || async move { Redirect::permanent(&root) }))
            .nest(&format!("{base}/"), mounted)
    }
}

async fn configuration(State(app): State<App>) -> axum::Json<serde_json::Value> {
    axum::Json(
        json!({"base_path":app.config.base_path,"version":job_protocol::VERSION,"api_base":format!("{}/api/v1",app.config.base_path),"auth_mode":app.config.auth_mode}),
    )
}
async fn ready(State(app): State<App>) -> StatusCode {
    if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    match tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(&app.pool),
    )
    .await
    {
        Ok(Ok(_)) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}
async fn shutdown(app: App) {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    app.shutting_down
        .store(true, std::sync::atomic::Ordering::Relaxed);
    for (_, session) in app.sessions.lock().await.drain() {
        session.close.cancel();
    }
    if let Err(error) = bounded(10, async {
        let _lock = app.dispatch_lock.lock().await;
        db::fail_active(
            &app.pool,
            None,
            "server_shutdown: worker connection was interrupted",
        )
        .await
    })
    .await
    {
        tracing::error!(%error,"shutdown status update failed; startup will reconcile active attempts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn serves_index_assets_and_redirect_under_nested_base_path() {
        let directory = std::env::temp_dir().join(format!("job-web-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(directory.join("assets"))
            .await
            .unwrap();
        tokio::fs::write(directory.join("index.html"), "INDEX")
            .await
            .unwrap();
        tokio::fs::write(directory.join("assets/app.js"), "SCRIPT")
            .await
            .unwrap();
        for base in ["", "/nested/jobs"] {
            let router: Router = mount_base_path(web_files(&directory), base);
            for (suffix, expected) in [
                ("/", "INDEX"),
                ("/index.html", "INDEX"),
                ("/assets/app.js", "SCRIPT"),
            ] {
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!("{base}{suffix}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{base}{suffix}");
                assert_eq!(
                    to_bytes(response.into_body(), 1024).await.unwrap(),
                    expected
                );
            }
            if !base.is_empty() {
                let response = router
                    .oneshot(Request::builder().uri(base).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
                assert_eq!(response.headers()["location"], format!("{base}/"));
            }
        }
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }
}
