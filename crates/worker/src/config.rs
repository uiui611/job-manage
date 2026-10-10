use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;
use std::{collections::BTreeMap, env, path::PathBuf};

pub const MAX_FILES: usize = 32;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_FILE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub struct Config {
    pub server_url: Url,
    pub token: String,
    pub worker_id: String,
    pub name: String,
    pub tags: Vec<String>,
    pub work_dir: PathBuf,
    pub node_path: String,
    pub python_path: String,
    pub runtime_env: BTreeMap<String, String>,
    pub allow_http: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let allow_http = env::var("JOB_ALLOW_INSECURE_HTTP").as_deref() == Ok("true");
        let server_url = Url::parse(&required("JOB_SERVER_URL")?)
            .context("JOB_SERVER_URL must be a valid URL")?;
        check_url(&server_url, allow_http)?;
        ensure!(
            server_url.query().is_none(),
            "JOB_SERVER_URL must not contain a query"
        );
        let worker_id = required("JOB_WORKER_ID")?;
        ensure!(
            worker_id.len() <= 120 && job_protocol::safe_file_name(&worker_id),
            "JOB_WORKER_ID must be a safe ASCII filename of at most 120 bytes"
        );
        let name = env::var("JOB_WORKER_NAME").unwrap_or_else(|_| worker_id.clone());
        ensure!(
            !name.is_empty() && name.len() <= 160,
            "JOB_WORKER_NAME must be 1..160 bytes"
        );
        let tags: Vec<String> =
            serde_json::from_str(&env::var("JOB_WORKER_TAGS").unwrap_or_else(|_| "[]".into()))
                .context("JOB_WORKER_TAGS must be a JSON array of strings")?;
        ensure!(
            tags.len() <= 64 && tags.iter().all(|s| !s.is_empty() && s.len() <= 128),
            "worker tags must be nonempty, at most 128 bytes each, and at most 64 tags"
        );
        let mut runtime_env = BTreeMap::new();
        runtime_env.insert(
            "PATH".into(),
            env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        );
        for key in [
            "LD_LIBRARY_PATH",
            "CUDA_VISIBLE_DEVICES",
            "NVIDIA_VISIBLE_DEVICES",
            "NVIDIA_DRIVER_CAPABILITIES",
            "ROCR_VISIBLE_DEVICES",
            "HIP_VISIBLE_DEVICES",
        ] {
            if let Ok(value) = env::var(key) {
                runtime_env.insert(key.into(), value);
            }
        }
        if let Ok(extra) = env::var("JOB_RUNTIME_ENV_ALLOWLIST") {
            for key in extra
                .split(',')
                .map(str::trim)
                .filter(|key| !key.is_empty())
            {
                ensure!(
                    !key.starts_with("JOB_")
                        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "runtime environment allowlist contains a forbidden name"
                );
                if let Ok(value) = env::var(key) {
                    runtime_env.insert(key.into(), value);
                }
            }
        }
        Ok(Self {
            token: required("JOB_WORKER_TOKEN")?,
            name,
            worker_id,
            tags,
            work_dir: PathBuf::from(env::var("JOB_WORK_DIR").unwrap_or_else(|_| "/work".into())),
            node_path: env::var("JOB_NODE_PATH").unwrap_or_else(|_| "node".into()),
            python_path: env::var("JOB_PYTHON_PATH").unwrap_or_else(|_| "python3".into()),
            runtime_env,
            server_url,
            allow_http,
        })
    }

    pub fn websocket_url(&self) -> Result<Url> {
        let mut url = self.server_url.clone();
        url.set_path(&format!("{}/worker", url.path().trim_end_matches('/')));
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid server URL scheme"))?;
        Ok(url)
    }

    pub fn transfer_url(&self, value: &str) -> Result<Url> {
        // Local storage uses signed origin-relative URLs; S3 uses absolute presigned URLs.
        let url = if value.starts_with('/') && !value.starts_with("//") {
            self.server_url
                .join(value)
                .context("invalid file transfer URL")?
        } else {
            Url::parse(value).context("file transfer URL must be absolute or origin-relative")?
        };
        check_url(&url, self.allow_http)?;
        Ok(url)
    }
}

fn required(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.trim().is_empty(), "{name} must not be empty");
    Ok(value)
}

fn check_url(url: &Url, allow_http: bool) -> Result<()> {
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        bail!("HTTPS is required (JOB_ALLOW_INSECURE_HTTP=true permits development HTTP)");
    }
    ensure!(url.host_str().is_some(), "URL must include a host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are not allowed"
    );
    ensure!(url.fragment().is_none(), "URL fragments are not allowed");
    Ok(())
}

#[cfg(test)]
pub fn test_config(path: PathBuf) -> Config {
    Config {
        server_url: Url::parse("https://example.test/jobs").unwrap(),
        token: "private-token".into(),
        worker_id: "test-worker".into(),
        name: "test".into(),
        tags: vec![],
        work_dir: path,
        node_path: "node".into(),
        python_path: "python3".into(),
        runtime_env: BTreeMap::from([("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into())]),
        allow_http: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transfer_urls_preserve_origin_and_require_tls() {
        let config = test_config(PathBuf::new());
        assert_eq!(
            config.websocket_url().unwrap().as_str(),
            "wss://example.test/jobs/worker"
        );
        assert_eq!(
            config
                .transfer_url("/jobs/api/v1/blobs/x?signature=123")
                .unwrap()
                .as_str(),
            "https://example.test/jobs/api/v1/blobs/x?signature=123"
        );
        assert!(config.transfer_url("http://example.test/unsafe").is_err());
        assert!(config.transfer_url("//other.test/unsafe").is_err());
        assert!(
            config
                .transfer_url("https://user:password@other.test/file")
                .is_err()
        );
        assert!(
            config
                .transfer_url("https://storage.test/file?signed=yes")
                .is_ok()
        );
    }
}
