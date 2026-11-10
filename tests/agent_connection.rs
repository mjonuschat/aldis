mod tls_support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use aldis::agent::api::{HostSnapshot, Snapshot};
use aldis::agent::connection::serve;
use aldis::agent::service::{AgentBackend, AgentService, EventSink};
use aldis::eligibility::CheckoutRevision;
use aldis::lock::{LockError, UpdateLock};
use aldis::moonraker::{HostInfo, KlippyState, McuInventory, PrintState};
use aldis::update_run::{RunFailure, RunHooks, RunOutcome};
use serde_json::{Value, json};
use tungstenite::Message;

struct Idle;

impl AgentBackend for Idle {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            host: HostSnapshot {
                url_is_local: true,
                klipper_unit: None,
                info: HostInfo {
                    state: KlippyState::Ready,
                    state_message: "Printer is ready".to_owned(),
                    software_version: Some("v2".to_owned()),
                    klipper_path: Some("/home/pi/klipper".into()),
                    python_path: None,
                },
                checkout_version: Some(CheckoutRevision::Known("v2".to_owned())),
                print_state: Some(PrintState::Standby),
                config_error: false,
            },
            inventory: Some(McuInventory {
                mcus: Vec::new(),
                unreported: Vec::new(),
            }),
            peers: Vec::new(),
            prebuilt: aldis::prebuilt::Targets::default(),
        }
    }
    fn try_lock(&self) -> Result<UpdateLock, LockError> {
        Err(LockError::Busy)
    }
    fn run(
        &self,
        _: &std::path::Path,
        _: UpdateLock,
        _: &Snapshot,
        _: &[String],
        _: bool,
        _: &mut dyn RunHooks,
    ) -> Result<RunOutcome, RunFailure> {
        unreachable!("no update is accepted in this test")
    }
    fn klipper_active(&self) -> Option<bool> {
        Some(true)
    }
    fn klippy_state(&self) -> KlippyState {
        KlippyState::Ready
    }
    fn run_log_path(&self, _: &str) -> PathBuf {
        PathBuf::from("/tmp/run.log")
    }
}

fn text(message: Message) -> Value {
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

#[test]
fn identifies_then_answers_relayed_requests_until_moonraker_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let moonraker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let identify = text(socket.read().unwrap());
        socket
            .send(Message::text(
                json!({"jsonrpc":"2.0","method":"status","id":7}).to_string(),
            ))
            .unwrap();
        let status = text(socket.read().unwrap());
        socket
            .send(Message::text(
                json!({"jsonrpc":"2.0","method":"update","params":{"all":true},"id":8}).to_string(),
            ))
            .unwrap();
        let busy = text(socket.read().unwrap());
        socket
            .send(Message::text(
                json!({"jsonrpc":"2.0","method":"reboot","id":9}).to_string(),
            ))
            .unwrap();
        let unknown = text(socket.read().unwrap());
        socket.close(None).unwrap();
        let _ = socket.read();
        (identify, status, busy, unknown)
    });
    let events: EventSink = Arc::new(Mutex::new(None));
    let service = AgentService::new(Idle, Arc::clone(&events));

    serve(&url, &aldis::tls::Tls::default(), &service, &events).expect("connection ends cleanly");
    let (identify, status, busy, unknown) = moonraker.join().unwrap();

    assert_eq!(identify["params"]["type"], "agent");
    assert_eq!(status["id"], 7);
    assert_eq!(status["result"]["api_version"], 1);
    assert_eq!(busy["error"]["data"]["reason"], "busy");
    assert_eq!(unknown["error"]["code"], -32601);
    assert!(
        events.lock().unwrap().is_none(),
        "sink is cleared when the connection ends"
    );
}

/// A Moonraker behind TLS that reads the agent's identify message, then closes. `None` when the
/// agent refused the certificate and the handshake never finished.
fn identify_over_tls(
    identity: &tls_support::Identity,
) -> (String, thread::JoinHandle<Option<Value>>) {
    let (address, server) = tls_support::serve_once(identity, |stream| {
        let mut socket = tungstenite::accept(stream).ok()?;
        let identify = text(socket.read().ok()?);
        socket.close(None).ok()?;
        let _ = socket.read();
        Some(identify)
    });
    (format!("https://{address}"), server)
}

fn serve_to(url: &str, tls: &aldis::tls::Tls) -> Result<(), tungstenite::Error> {
    let events: EventSink = Arc::new(Mutex::new(None));
    let service = AgentService::new(Idle, Arc::clone(&events));
    serve(url, tls, &service, &events)
}

fn trusting(pem: &str) -> (tempfile::TempDir, aldis::tls::Tls) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("moonraker.pem");
    std::fs::write(&path, pem).unwrap();
    let tls = aldis::tls::Tls::new(&aldis::tls::TlsOptions {
        ca_file: Some(path),
        insecure_skip_verify: false,
    })
    .unwrap();
    (directory, tls)
}

#[test]
fn connects_to_a_wss_moonraker_whose_certificate_is_in_ca_file() {
    let identity = tls_support::identity();
    let (_directory, tls) = trusting(&identity.certificate_pem);
    let (url, server) = identify_over_tls(&identity);

    serve_to(&url, &tls).expect("connection ends cleanly");

    assert_eq!(server.join().unwrap().unwrap()["params"]["type"], "agent");
}

#[test]
fn refuses_a_wss_moonraker_with_an_untrusted_certificate() {
    let identity = tls_support::identity();
    let (url, server) = identify_over_tls(&identity);

    assert!(serve_to(&url, &aldis::tls::Tls::default()).is_err());
    assert_eq!(server.join().unwrap(), None);
}

#[test]
fn connects_to_a_wss_moonraker_when_verification_is_skipped() {
    let identity = tls_support::identity();
    let tls = aldis::tls::Tls::new(&aldis::tls::TlsOptions {
        ca_file: None,
        insecure_skip_verify: true,
    })
    .unwrap();
    let (url, server) = identify_over_tls(&identity);

    serve_to(&url, &tls).expect("connection ends cleanly");

    assert_eq!(server.join().unwrap().unwrap()["params"]["type"], "agent");
}
