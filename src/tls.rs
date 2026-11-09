//! How aldis verifies the certificate of an `https://` Moonraker.
//!
//! One [`Tls`] drives both channels to Moonraker, the REST client (`ureq`) and the agent's
//! websocket (`tungstenite`), so they always agree on what they trust.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsOptions {
    /// A PEM file of the certificates to trust instead of the built-in public roots, such as the
    /// private CA that signed Moonraker's certificate, or Moonraker's own self-signed one.
    pub ca_file: Option<PathBuf>,
    /// Accept any certificate without checking it. This removes the protection TLS gives against
    /// an attacker on the network path.
    pub insecure_skip_verify: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("set either ca_file or insecure_skip_verify, not both")]
    Conflict,
    #[error("could not read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a valid PEM certificate file: {source}", path.display())]
    Pem {
        path: PathBuf,
        #[source]
        source: ureq::Error,
    },
    #[error("{} contains no PEM certificate", .0.display())]
    NoCertificate(PathBuf),
    #[error("{} holds a certificate that cannot be trusted: {source}", path.display())]
    BadCertificate {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },
}

#[derive(Clone)]
pub struct Tls {
    rest: ureq::tls::TlsConfig,
    websocket: Arc<ClientConfig>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tls").finish_non_exhaustive()
    }
}

impl Default for Tls {
    fn default() -> Self {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Self {
            rest: ureq::tls::TlsConfig::default(),
            websocket: client_config(roots),
        }
    }
}

impl Tls {
    pub fn new(options: &TlsOptions) -> Result<Self, TlsError> {
        match (&options.ca_file, options.insecure_skip_verify) {
            (Some(_), true) => Err(TlsError::Conflict),
            (None, false) => Ok(Self::default()),
            (None, true) => Ok(Self {
                rest: ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
                websocket: insecure_client_config(),
            }),
            (Some(path), false) => Self::trusting(path),
        }
    }

    fn trusting(path: &Path) -> Result<Self, TlsError> {
        let certificates = read_certificates(path)?;
        let mut roots = RootCertStore::empty();
        for certificate in &certificates {
            roots
                .add(CertificateDer::from(certificate.der().to_vec()))
                .map_err(|source| TlsError::BadCertificate {
                    path: path.to_owned(),
                    source,
                })?;
        }
        Ok(Self {
            rest: ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certificates))
                .build(),
            websocket: client_config(roots),
        })
    }

    pub(crate) fn rest_config(&self) -> ureq::tls::TlsConfig {
        self.rest.clone()
    }

    pub(crate) fn websocket_connector(&self) -> tungstenite::Connector {
        tungstenite::Connector::Rustls(Arc::clone(&self.websocket))
    }
}

fn read_certificates(path: &Path) -> Result<Vec<ureq::tls::Certificate<'static>>, TlsError> {
    let text = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(&text) {
        let item = item.map_err(|source| TlsError::Pem {
            path: path.to_owned(),
            source,
        })?;
        if let ureq::tls::PemItem::Certificate(certificate) = item {
            certificates.push(certificate);
        }
    }
    if certificates.is_empty() {
        return Err(TlsError::NoCertificate(path.to_owned()));
    }
    Ok(certificates)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn insecure_client_config() -> Arc<ClientConfig> {
    let provider = provider();
    Arc::new(
        ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth(),
    )
}

/// Accepts every server certificate. The handshake signatures are still checked, so the
/// connection is encrypted, but the peer is not authenticated.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
