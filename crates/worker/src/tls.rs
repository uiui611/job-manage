use anyhow::{Context, Result, ensure};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use std::{fs::File, io::Read, path::Path, sync::Arc, time::Duration};

const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;

pub struct Transport {
    pub http: reqwest::Client,
    pub websocket: Option<tokio_tungstenite::Connector>,
}

impl Transport {
    pub fn new(ca_file: Option<&Path>) -> Result<Self> {
        let mut http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300));
        let websocket = if let Some(path) = ca_file {
            let certificates = read_ca_certificates(path)?;
            let roots = websocket_roots(&certificates)?;
            for certificate in certificates {
                // Adding a root retains reqwest's normal built-in trust store.
                http = http.add_root_certificate(
                    reqwest::Certificate::from_der(certificate.as_ref())
                        .context("JOB_CA_CERT_FILE contains an invalid certificate")?,
                );
            }
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .context("cannot initialize TLS protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls)))
        } else {
            // Preserve tokio-tungstenite's normal TLS behavior when unset.
            None
        };
        Ok(Self {
            http: http.build().context("cannot initialize HTTPS client")?,
            websocket,
        })
    }
}

fn read_ca_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut bytes = Vec::new();
    File::open(path)
        .context("cannot open JOB_CA_CERT_FILE")?
        .take(MAX_CA_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("cannot read JOB_CA_CERT_FILE")?;
    ensure!(
        bytes.len() as u64 <= MAX_CA_FILE_BYTES,
        "JOB_CA_CERT_FILE exceeds 1 MiB"
    );
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("JOB_CA_CERT_FILE contains malformed PEM")?;
    ensure!(
        !certificates.is_empty(),
        "JOB_CA_CERT_FILE must contain at least one PEM certificate"
    );
    Ok(certificates)
}

fn websocket_roots(certificates: &[CertificateDer<'static>]) -> Result<RootCertStore> {
    // Match the existing rustls-tls-webpki-roots feature, then append custom CAs.
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for certificate in certificates {
        roots
            .add(certificate.clone())
            .context("JOB_CA_CERT_FILE contains an invalid X.509 certificate")?;
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };
    use tokio_tungstenite::tungstenite::Message;

    fn private_ca() -> (rcgen::Certificate, Issuer<'static, KeyPair>) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        (certificate, Issuer::new(params, key))
    }

    fn server_config(issuer: &Issuer<'_, KeyPair>) -> Arc<rustls::ServerConfig> {
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, issuer).unwrap();
        Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap(),
        )
    }

    async fn tls_server(
        config: Arc<rustls::ServerConfig>,
        websocket: bool,
    ) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(Ok(mut tls)) =
                        tokio::time::timeout(Duration::from_secs(3), acceptor.accept(stream)).await
                    else {
                        return;
                    };
                    if websocket {
                        let mut socket = tokio_tungstenite::accept_async(tls).await.unwrap();
                        if let Some(Ok(message)) = socket.next().await {
                            socket.send(message).await.unwrap();
                        }
                    } else {
                        let mut buffer = [0; 4096];
                        if tls.read(&mut buffer).await.unwrap_or(0) > 0 {
                            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
                            let _ = tls.shutdown().await;
                        }
                    }
                });
            }
        });
        (port, task)
    }

    #[test]
    fn rejects_unreadable_empty_malformed_and_invalid_ca_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.pem");
        assert!(Transport::new(Some(&path)).is_err());
        for contents in [
            "",
            "not a certificate",
            "-----BEGIN CERTIFICATE-----\ninvalid!\n-----END CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(Transport::new(Some(&path)).is_err());
        }
        std::fs::write(&path, vec![b' '; MAX_CA_FILE_BYTES as usize + 1]).unwrap();
        assert!(Transport::new(Some(&path)).is_err());
    }

    #[tokio::test]
    async fn extra_ca_trusts_https_and_wss_without_disabling_verification() {
        let (ca, issuer) = private_ca();
        let (unrelated_ca, _) = private_ca();
        let directory = tempfile::tempdir().unwrap();
        let ca_path = directory.path().join("ca.pem");
        // Exercise a bundle, including a certificate that did not sign the server.
        std::fs::write(&ca_path, format!("{}{}", unrelated_ca.pem(), ca.pem())).unwrap();
        let certificates = read_ca_certificates(&ca_path).unwrap();
        assert_eq!(certificates.len(), 2);
        let roots = websocket_roots(&certificates).unwrap();
        assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len() + 2);
        assert!(
            webpki_roots::TLS_SERVER_ROOTS
                .iter()
                .all(|root| roots.roots.contains(root))
        );
        let trusted = Transport::new(Some(&ca_path)).unwrap();
        let untrusted = Transport::new(None).unwrap();
        let config = server_config(&issuer);
        let (http_port, http_task) = tls_server(config.clone(), false).await;
        let (ws_port, ws_task) = tls_server(config, true).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            let url = format!("https://localhost:{http_port}/");
            assert!(untrusted.http.get(&url).send().await.is_err());
            assert_eq!(
                trusted
                    .http
                    .get(&url)
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "ok"
            );
            // Trusted CA must not allow a certificate for the wrong hostname.
            assert!(
                trusted
                    .http
                    .get(format!("https://127.0.0.1:{http_port}/"))
                    .send()
                    .await
                    .is_err()
            );
            let url = format!("wss://localhost:{ws_port}/");
            assert!(
                tokio_tungstenite::connect_async_tls_with_config(
                    &url,
                    None,
                    false,
                    untrusted.websocket
                )
                .await
                .is_err()
            );
            let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
                &url,
                None,
                false,
                trusted.websocket.clone(),
            )
            .await
            .unwrap();
            socket.send(Message::Text("verified".into())).await.unwrap();
            assert_eq!(
                socket.next().await.unwrap().unwrap().into_text().unwrap(),
                "verified"
            );
            assert!(
                tokio_tungstenite::connect_async_tls_with_config(
                    format!("wss://127.0.0.1:{ws_port}/"),
                    None,
                    false,
                    trusted.websocket
                )
                .await
                .is_err()
            );
        })
        .await
        .expect("local TLS checks timed out");
        http_task.abort();
        ws_task.abort();
    }
}
