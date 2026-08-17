//! Cluster mTLS from PEM env vars (not directories). Required for every clustered TCP path.

use std::io::{Error as IoError, ErrorKind};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Once;
use std::task::{Context, Poll};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub fn tls_required() -> bool {
    true
}

fn env_pem(name: &str) -> Result<String, IoError> {
    #[cfg(test)]
    ensure_test_tls_env();
    std::env::var(name)
        .map_err(|_| {
            IoError::new(
                ErrorKind::NotFound,
                format!("{name} is required for cluster TLS"),
            )
        })
        .and_then(|value| {
            if value.trim().is_empty() {
                Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("{name} is empty"),
                ))
            } else {
                Ok(value)
            }
        })
}

#[cfg(test)]
fn ensure_test_tls_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var("SKIPPR_CLUSTER_TLS_CERT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        {
            std::env::set_var(
                "SKIPPR_CLUSTER_TLS_CERT",
                include_str!("../../tests/cluster_tls/node.pem"),
            );
            std::env::set_var(
                "SKIPPR_CLUSTER_TLS_KEY",
                include_str!("../../tests/cluster_tls/node.key"),
            );
            std::env::set_var(
                "SKIPPR_CLUSTER_TLS_CA",
                include_str!("../../tests/cluster_tls/ca.pem"),
            );
        }
    });
}

fn parse_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, IoError> {
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    rustls_pemfile::certs(&mut cursor)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))
}

fn parse_key(pem: &str) -> Result<PrivateKeyDer<'static>, IoError> {
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut cursor)
        .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?
        .ok_or_else(|| {
            IoError::new(
                ErrorKind::InvalidData,
                "cluster TLS key PEM has no private key",
            )
        })
}

fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn server_config() -> Result<Arc<ServerConfig>, IoError> {
    ensure_crypto_provider();
    let certs = parse_certs(&env_pem("SKIPPR_CLUSTER_TLS_CERT")?)?;
    let key = parse_key(&env_pem("SKIPPR_CLUSTER_TLS_KEY")?)?;
    let mut roots = RootCertStore::empty();
    for cert in parse_certs(&env_pem("SKIPPR_CLUSTER_TLS_CA")?)? {
        roots
            .add(cert)
            .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?;
    }
    let config = ServerConfig::builder()
        .with_client_cert_verifier(
            rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?,
        )
        .with_single_cert(certs, key)
        .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?;
    Ok(Arc::new(config))
}

pub fn client_config() -> Result<Arc<ClientConfig>, IoError> {
    ensure_crypto_provider();
    let mut roots = RootCertStore::empty();
    for cert in parse_certs(&env_pem("SKIPPR_CLUSTER_TLS_CA")?)? {
        roots
            .add(cert)
            .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?;
    }
    let certs = parse_certs(&env_pem("SKIPPR_CLUSTER_TLS_CERT")?)?;
    let key = parse_key(&env_pem("SKIPPR_CLUSTER_TLS_KEY")?)?;
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|err| IoError::new(ErrorKind::InvalidData, err.to_string()))?;
    Ok(Arc::new(config))
}

pub fn tonic_server_tls() -> Result<tonic::transport::ServerTlsConfig, IoError> {
    ensure_crypto_provider();
    let cert = env_pem("SKIPPR_CLUSTER_TLS_CERT")?;
    let key = env_pem("SKIPPR_CLUSTER_TLS_KEY")?;
    let ca = env_pem("SKIPPR_CLUSTER_TLS_CA")?;
    Ok(tonic::transport::ServerTlsConfig::new()
        .identity(tonic::transport::Identity::from_pem(cert, key))
        .client_ca_root(tonic::transport::Certificate::from_pem(ca))
        .client_auth_optional(false))
}

pub fn tonic_client_tls() -> Result<tonic::transport::ClientTlsConfig, IoError> {
    ensure_crypto_provider();
    let ca = env_pem("SKIPPR_CLUSTER_TLS_CA")?;
    let cert = env_pem("SKIPPR_CLUSTER_TLS_CERT")?;
    let key = env_pem("SKIPPR_CLUSTER_TLS_KEY")?;
    Ok(tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca))
        .identity(tonic::transport::Identity::from_pem(cert, key))
        .domain_name("skippr-cluster"))
}

pub enum MaybeTlsStream {
    Server(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    Client(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

pub async fn accept(stream: TcpStream) -> Result<MaybeTlsStream, IoError> {
    let acceptor = TlsAcceptor::from(server_config()?);
    let tls = acceptor.accept(stream).await?;
    Ok(MaybeTlsStream::Server(Box::new(tls)))
}

pub async fn connect(stream: TcpStream) -> Result<MaybeTlsStream, IoError> {
    let connector = TlsConnector::from(client_config()?);
    let tls = connector
        .connect(
            ServerName::try_from("skippr-cluster")
                .map_err(|err| IoError::new(ErrorKind::InvalidInput, err.to_string()))?,
            stream,
        )
        .await?;
    Ok(MaybeTlsStream::Client(Box::new(tls)))
}

macro_rules! pin_project_stream {
    ($self:ident, $method:ident, $($args:tt)*) => {
        match $self.get_mut() {
            MaybeTlsStream::Server(s) => Pin::new(s.as_mut()).$method($($args)*),
            MaybeTlsStream::Client(s) => Pin::new(s.as_mut()).$method($($args)*),
        }
    };
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        pin_project_stream!(self, poll_read, cx, buf)
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        pin_project_stream!(self, poll_write, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        pin_project_stream!(self, poll_flush, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        pin_project_stream!(self, poll_shutdown, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clustered_tcp_always_requires_tls() {
        assert!(tls_required());
        let _ = server_config().expect("test PEMs");
    }
}
