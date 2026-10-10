use crate::App;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    !a.is_empty()
        && !b.is_empty()
        && bool::from(Sha256::digest(a.as_bytes()).ct_eq(&Sha256::digest(b.as_bytes())))
}
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
pub async fn check_token(app: &App, token: &str, kind: &str) -> Option<String> {
    let bootstrap = if kind == "worker" {
        &app.config.worker_token
    } else {
        &app.config.api_token
    };
    if equal(token, bootstrap) {
        return Some(format!("bootstrap-{kind}"));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM tokens WHERE token_hash=$1 AND kind=$2 AND revoked=FALSE",
        )
        .bind(hash(token))
        .bind(kind)
        .fetch_optional(&app.pool),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten()
}
pub async fn api_auth(State(app): State<App>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let token_ok = if let Some(token) = bearer(headers) {
        check_token(&app, token, "api").await.is_some()
    } else {
        false
    };
    let proxy_ok = headers
        .get("x-job-proxy-token")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|token| equal(token, &app.config.proxy_token));
    if !token_ok && !proxy_ok && app.config.auth_mode != "none" {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error":"API token or authenticated proxy required"})),
        )
            .into_response();
    }
    let mutation = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if !token_ok
        && mutation
        && (headers.get("x-job-request").and_then(|h| h.to_str().ok()) != Some("1")
            || headers.get("sec-fetch-site").and_then(|h| h.to_str().ok()) == Some("cross-site"))
    {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error":"Same-origin X-Job-Request header required"})),
        )
            .into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(15), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"error":"Operation timed out; query run status before resubmitting"}))).into_response(),
    }
}
