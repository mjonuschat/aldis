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
                },
                checkout_version: Some(CheckoutRevision::Known("v2".to_owned())),
                print_state: Some(PrintState::Standby),
                config_error: false,
            },
            inventory: Some(McuInventory {
                mcus: Vec::new(),
                unreported: Vec::new(),
            }),
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

    serve(&url, &service, &events).expect("connection ends cleanly");
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
