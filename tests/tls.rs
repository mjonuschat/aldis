mod tls_support;

use std::io::{Read, Write};
use std::time::Duration;

use aldis::moonraker::{HostPort, MoonrakerAdapter};
use aldis::tls::{Tls, TlsError, TlsOptions};
use tls_support::{TlsStream, identity, serve_once};

const BODY: &str = r#"{"result":{"system_info":{"instance_ids":{"klipper":"klipper-2"}}}}"#;

/// Answers one HTTP request with a canned Moonraker reply, or `None` when the TLS handshake fails.
fn reply(mut stream: TlsStream) -> Option<()> {
    let mut request = [0_u8; 1024];
    if stream.read(&mut request).ok()? == 0 {
        return None;
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    stream.write_all(response.as_bytes()).ok()?;
    stream.flush().ok()
}

fn ask(address: std::net::SocketAddr, tls: &Tls) -> Result<Option<String>, String> {
    MoonrakerAdapter::with_tls(format!("https://{address}"), Duration::from_secs(5), tls)
        .klipper_unit()
        .map_err(|error| aldis::error_chain(&error))
}

fn options_trusting(pem: &str) -> (tempfile::TempDir, TlsOptions) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("moonraker.pem");
    std::fs::write(&path, pem).unwrap();
    (
        directory,
        TlsOptions {
            ca_file: Some(path),
            insecure_skip_verify: false,
        },
    )
}

#[test]
fn rest_rejects_a_self_signed_certificate_by_default() {
    let identity = identity();
    let (address, server) = serve_once(&identity, reply);

    assert!(ask(address, &Tls::default()).is_err());
    assert_eq!(server.join().unwrap(), None, "the handshake must fail");
}

#[test]
fn rest_trusts_the_certificate_in_ca_file() {
    let identity = identity();
    let (_directory, options) = options_trusting(&identity.certificate_pem);
    let (address, server) = serve_once(&identity, reply);

    let unit = ask(address, &Tls::new(&options).unwrap());

    assert_eq!(unit, Ok(Some("klipper-2".to_owned())));
    assert_eq!(server.join().unwrap(), Some(()));
}

#[test]
fn rest_rejects_a_certificate_the_ca_file_does_not_hold() {
    let served = identity();
    let (_directory, options) = options_trusting(&self::identity().certificate_pem);
    let (address, server) = serve_once(&served, reply);

    assert!(ask(address, &Tls::new(&options).unwrap()).is_err());
    assert_eq!(server.join().unwrap(), None);
}

#[test]
fn rest_accepts_any_certificate_when_verification_is_skipped() {
    let identity = identity();
    let (address, server) = serve_once(&identity, reply);
    let options = TlsOptions {
        ca_file: None,
        insecure_skip_verify: true,
    };

    assert_eq!(
        ask(address, &Tls::new(&options).unwrap()),
        Ok(Some("klipper-2".to_owned()))
    );
    assert_eq!(server.join().unwrap(), Some(()));
}

#[test]
fn refuses_both_a_ca_file_and_skipping_verification() {
    let options = TlsOptions {
        ca_file: Some("/etc/ssl/moonraker.pem".into()),
        insecure_skip_verify: true,
    };

    assert!(matches!(Tls::new(&options), Err(TlsError::Conflict)));
}

#[test]
fn reports_a_missing_ca_file() {
    let options = TlsOptions {
        ca_file: Some("/nonexistent/moonraker.pem".into()),
        insecure_skip_verify: false,
    };

    let error = Tls::new(&options).unwrap_err();

    assert!(matches!(error, TlsError::Read { .. }), "{error}");
    assert!(error.to_string().contains("/nonexistent/moonraker.pem"));
}

#[test]
fn reports_a_ca_file_without_a_certificate() {
    let (_directory, options) = options_trusting("not a certificate\n");

    let error = Tls::new(&options).unwrap_err();

    assert!(matches!(error, TlsError::NoCertificate(_)), "{error}");
}
