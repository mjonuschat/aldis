//! A throwaway TLS server for tests: a self-signed certificate for 127.0.0.1 and a listener that
//! hands each accepted, handshaken connection to a closure.
#![allow(dead_code)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

pub type TlsStream = StreamOwned<ServerConnection, TcpStream>;

pub struct Identity {
    pub certificate_pem: String,
    config: Arc<ServerConfig>,
}

pub fn identity() -> Identity {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
    Identity {
        certificate_pem: certified.cert.pem(),
        config: Arc::new(config),
    }
}

/// Accepts one connection and passes it to `handler`. The handshake completes inside `handler`'s
/// first read or write, so a client that rejects the certificate makes that call fail.
pub fn serve_once<T, F>(identity: &Identity, handler: F) -> (SocketAddr, JoinHandle<T>)
where
    T: Send + 'static,
    F: FnOnce(TlsStream) -> T + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = Arc::clone(&identity.config);
    let thread = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handler(StreamOwned::new(
            ServerConnection::new(config).unwrap(),
            stream,
        ))
    });
    (address, thread)
}
