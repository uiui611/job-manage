use anyhow::{Result, bail};
use std::{env, path::PathBuf};

#[derive(Clone)]
pub struct Config {
    pub bind: String,
    pub database_url: String,
    pub base_path: String,
    pub web_dir: PathBuf,
    pub storage_dir: PathBuf,
    pub api_token: String,
    pub worker_token: String,
    pub proxy_token: String,
    pub auth_mode: String,
    pub signing_key: String,
    pub heartbeat_timeout: u64,
    pub max_artifact_bytes: u64,
}
impl Config {
    pub fn from_env() -> Result<Self> {
        let base_path = normalize_base(&env::var("JOB_BASE_PATH").unwrap_or_default())?;
        let auth_mode = env::var("JOB_AUTH_MODE").unwrap_or_else(|_| "proxy".into());
        if !["proxy", "none"].contains(&auth_mode.as_str()) {
            bail!("JOB_AUTH_MODE must be proxy or none");
        }
        let api_token = env::var("JOB_API_TOKEN").unwrap_or_default();
        let worker_token = env::var("JOB_WORKER_TOKEN").unwrap_or_default();
        let proxy_token = env::var("JOB_PROXY_TOKEN").unwrap_or_default();
        if auth_mode != "none" && api_token.len() < 32 {
            bail!("JOB_API_TOKEN must be at least 32 characters");
        }
        if worker_token.len() < 32 {
            bail!("JOB_WORKER_TOKEN must be at least 32 characters");
        }
        if !proxy_token.is_empty() && proxy_token.len() < 32 {
            bail!("JOB_PROXY_TOKEN must be at least 32 characters");
        }
        let heartbeat_timeout = env::var("JOB_HEARTBEAT_TIMEOUT_SECONDS")
            .unwrap_or_else(|_| "30".into())
            .parse::<u64>()?;
        if !(5..=300).contains(&heartbeat_timeout) {
            bail!("heartbeat timeout must be 5..300 seconds");
        }
        Ok(Self {
            bind: env::var("JOB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: env::var("JOB_DATABASE_URL")?,
            base_path,
            web_dir: env::var("JOB_WEB_DIR")
                .unwrap_or_else(|_| "web/dist".into())
                .into(),
            storage_dir: env::var("JOB_STORAGE_DIR")
                .unwrap_or_else(|_| "data".into())
                .into(),
            api_token,
            worker_token,
            proxy_token,
            auth_mode,
            signing_key: env::var("JOB_SIGNING_KEY")
                .unwrap_or_else(|_| format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4())),
            heartbeat_timeout,
            max_artifact_bytes: 64 * 1024 * 1024,
        })
    }
}
pub fn normalize_base(path: &str) -> Result<String> {
    if path.is_empty() || path == "/" {
        return Ok(String::new());
    }
    let path = path.trim_end_matches('/');
    if ["/healthz", "/readyz"].contains(&path) {
        bail!("JOB_BASE_PATH conflicts with a reserved health endpoint");
    }
    if !path.starts_with('/')
        || path.contains("//")
        || path.split('/').skip(1).any(|s| {
            s.is_empty()
                || s == "."
                || s == ".."
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
        })
    {
        bail!("JOB_BASE_PATH must be an absolute URL path of alphanumeric, _ or - segments");
    }
    Ok(path.to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base_paths() {
        assert_eq!(normalize_base("/apps/jobs/").unwrap(), "/apps/jobs");
        assert_eq!(normalize_base("/").unwrap(), "");
        for p in [
            "jobs",
            "/../jobs",
            "/a//b",
            "/<script>",
            "/a?b",
            "/healthz",
            "/readyz/",
        ] {
            assert!(normalize_base(p).is_err());
        }
    }
}
