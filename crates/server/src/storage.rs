use crate::{config::Config, db::Artifact};
use anyhow::{Result, bail};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use object_store::{
    ObjectStore,
    aws::{AmazonS3, AmazonS3Builder},
    path::Path,
    signer::Signer,
};
use sha2::Sha256;
use std::{env, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct Storage {
    pub local: PathBuf,
    pub s3: Option<Arc<AmazonS3>>,
    pub signer: Option<Arc<AmazonS3>>,
    pub base_path: String,
    signing_key: String,
}
impl Storage {
    pub async fn new(config: &Config) -> Result<Self> {
        let local = config.storage_dir.join("artifacts");
        tokio::fs::create_dir_all(&local).await?;
        let (s3, signer) = if let Ok(endpoint) = env::var("JOB_S3_ENDPOINT") {
            let build = |endpoint: String| -> Result<AmazonS3> {
                Ok(AmazonS3Builder::new()
                    .with_endpoint(endpoint.clone())
                    .with_allow_http(endpoint.starts_with("http://"))
                    .with_virtual_hosted_style_request(false)
                    .with_region(env::var("JOB_S3_REGION").unwrap_or_else(|_| "us-east-1".into()))
                    .with_bucket_name(env::var("JOB_S3_BUCKET")?)
                    .with_access_key_id(env::var("JOB_S3_ACCESS_KEY")?)
                    .with_secret_access_key(env::var("JOB_S3_SECRET_KEY")?)
                    .build()?)
            };
            let store = Arc::new(build(endpoint.clone())?);
            let signer = Arc::new(build(
                env::var("JOB_S3_PUBLIC_ENDPOINT").unwrap_or(endpoint),
            )?);
            (Some(store), Some(signer))
        } else {
            (None, None)
        };
        Ok(Self {
            local,
            s3,
            signer,
            base_path: config.base_path.clone(),
            signing_key: config.signing_key.clone(),
        })
    }
    pub fn kind(&self) -> &'static str {
        if self.s3.is_some() { "s3" } else { "local" }
    }
    pub async fn url(&self, artifact: &Artifact, method: http::Method) -> Result<String> {
        if let Some(s3) = &self.signer {
            return Ok(s3
                .signed_url(
                    method,
                    &Path::from(artifact.object_key.clone()),
                    Duration::from_secs(3600),
                )
                .await?
                .to_string());
        }
        let expires = chrono::Utc::now().timestamp() + 3600;
        let signature = self.signature(method.as_str(), &artifact.id, expires);
        Ok(format!(
            "{}/api/v1/blobs/{}?expires={expires}&signature={signature}",
            self.base_path, artifact.id
        ))
    }
    fn signature(&self, method: &str, id: &str, expires: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.signing_key.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(format!("{method}\n{id}\n{expires}").as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    pub fn verify(&self, method: &str, id: &str, expires: i64, signature: &str) -> bool {
        use subtle::ConstantTimeEq;
        expires >= chrono::Utc::now().timestamp()
            && expires <= chrono::Utc::now().timestamp() + 3700
            && bool::from(
                self.signature(method, id, expires)
                    .as_bytes()
                    .ct_eq(signature.as_bytes()),
            )
    }
    pub async fn verify_size(&self, artifact: &Artifact) -> Result<()> {
        let size = if let Some(s3) = &self.s3 {
            s3.head(&Path::from(artifact.object_key.clone()))
                .await?
                .size
        } else {
            tokio::fs::metadata(self.local.join(&artifact.id))
                .await?
                .len()
        };
        if size != artifact.size as u64 {
            bail!("Uploaded size does not match declared size");
        }
        Ok(())
    }
    pub async fn put_local(&self, artifact: &Artifact, data: Bytes) -> Result<()> {
        if self.s3.is_some() {
            bail!("Local blob route is disabled with S3 storage");
        }
        if data.len() as i64 != artifact.size {
            bail!("Uploaded size does not match declared size");
        }
        let temp = self
            .local
            .join(format!("{}.{}.tmp", artifact.id, uuid::Uuid::new_v4()));
        tokio::fs::write(&temp, data).await?;
        tokio::fs::rename(temp, self.local.join(&artifact.id)).await?;
        Ok(())
    }
}
