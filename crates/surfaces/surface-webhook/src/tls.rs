//! HTTPS termination: the same policy and code as surface-web's `tls` module. The operator
//! supplies the PEM pair (`aho cert` writes one); handshakes run off the accept loop so one slow
//! or plain-HTTP client cannot stall the listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn server_config(cert_pem: &str, key_pem: &str) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("the certificate PEM does not parse ({e}); pass the file `aho cert` wrote"))?;
    if certs.is_empty() {
        return Err("the certificate PEM holds no CERTIFICATE block; pass the file `aho cert` wrote".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| format!("the private key PEM does not parse ({e}); pass the key file `aho cert` wrote"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the certificate and key do not form a usable pair ({e}); regenerate them with `aho cert`"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// A listener yielding completed TLS streams; failed handshakes are dropped.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, cfg: Arc<rustls::ServerConfig>) -> Result<TlsListener, String> {
        let local = tcp.local_addr().map_err(|e| format!("the listener has no address: {e}"))?;
        let acceptor = TlsAcceptor::from(cfg);
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(x) => x,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    break;
                }
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(tls)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        // A send error means the server stopped; the stream is dropped with it.
                        tx.send((tls, peer)).await.unwrap_or_default();
                    }
                });
            }
        });
        Ok(TlsListener { rx, local })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(x) => x,
            // The accept loop only ends when this listener is dropped; park instead of spinning.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The remote address for `ConnectInfo` (log lines), over TLS or plain TCP.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Peer {
        Peer(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Peer {
        Peer(*stream.remote_addr())
    }
}
