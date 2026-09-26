//! Request validation, the single update worker, and the in-memory run record.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::api::{
    Action, McuOutcome, McuOutcomeBody, Outcome, Phase, RunBody, RunResultBody, RunState, Snapshot,
    StatusBody, UpdateResponse, assess, display_name, klippy_state_label,
};
use crate::coordinator::UpdateProgress;
use crate::flash_order::flash_order;
use crate::lock::{LockError, UpdateLock};
use crate::moonraker::KlippyState;
use crate::update_run::{RunFailure, RunHooks, RunOutcome, RunStep};

const MESSAGE_CAP: usize = 500;
const REJECTED: i64 = -32000;
const INVALID_PARAMS: i64 = -32602;

/// Hardware and Moonraker access, isolated so the service can be tested with fakes.
pub trait AgentBackend: Send + Sync + 'static {
    fn snapshot(&self) -> Snapshot;
    fn try_lock(&self) -> Result<UpdateLock, LockError>;
    /// Runs the update, writing its detailed log to `log`.
    fn run(
        &self,
        log: &Path,
        lock: UpdateLock,
        snapshot: &Snapshot,
        targets: &[String],
        hooks: &mut dyn RunHooks,
    ) -> Result<RunOutcome, RunFailure>;
    /// Whether the `klipper` unit is active; `None` when it cannot be queried.
    fn klipper_active(&self) -> Option<bool>;
    fn klippy_state(&self) -> KlippyState;
    /// Chooses (and creates) the run's log file.
    fn run_log_path(&self, run_id: &str) -> PathBuf;
}

/// Where the current websocket connection receives events; `None` while disconnected, in which
/// case events are only kept in the run record.
pub type EventSink = Arc<Mutex<Option<mpsc::Sender<UpdateResponse>>>>;

/// A JSON-RPC error object returned to the frontend (wrapped by Moonraker).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiError {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl ApiError {
    pub fn new(code: i64, reason: &str, message: impl Into<String>, extra: Value) -> Self {
        let mut data = json!({ "reason": reason });
        if let (Some(data), Value::Object(extra)) = (data.as_object_mut(), extra) {
            data.extend(extra);
        }
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    fn rejected(reason: &str, message: impl Into<String>) -> Self {
        Self::new(REJECTED, reason, message, Value::Null)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum UpdateRequest {
    Mcus { mcus: Vec<String> },
    All { all: bool },
}

#[derive(Default)]
struct ServiceState {
    run: Option<RunBody>,
    /// The assessment taken when the running update was accepted.
    run_snapshot: Option<StatusBody>,
}

pub struct AgentService<B: AgentBackend> {
    backend: Arc<B>,
    state: Arc<(Mutex<ServiceState>, Condvar)>,
    events: EventSink,
}

impl<B: AgentBackend> Clone for AgentService<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            state: Arc::clone(&self.state),
            events: Arc::clone(&self.events),
        }
    }
}

fn push_capped(run: &mut RunBody, message: UpdateResponse) {
    if run.messages.len() >= MESSAGE_CAP {
        run.messages.remove(0);
    }
    run.messages.push(message);
}

impl<B: AgentBackend> AgentService<B> {
    pub fn new(backend: B, events: EventSink) -> Self {
        Self {
            backend: Arc::new(backend),
            state: Arc::default(),
            events,
        }
    }

    pub fn status(&self) -> StatusBody {
        let state = self.state.0.lock().unwrap();
        if let (Some(run), Some(snapshot)) = (&state.run, &state.run_snapshot)
            && run.state == RunState::Running
        {
            let mut body = snapshot.clone();
            for mcu in &mut body.mcus {
                mcu.actions.clear();
            }
            body.run = Some(run.clone());
            return body;
        }
        let run = state.run.clone();
        drop(state);
        let mut body = assess(&self.backend.snapshot());
        body.run = run;
        body
    }

    pub fn update(&self, request: UpdateRequest) -> Result<Value, ApiError> {
        let lock = self.backend.try_lock().map_err(|error| match error {
            LockError::Busy => ApiError::rejected("busy", "an update is already running"),
            error => ApiError::rejected("unavailable", error.to_string()),
        })?;
        let snapshot = self.backend.snapshot();
        let body = assess(&snapshot);
        if let Some(blocker) = &body.blocker {
            return Err(ApiError::new(
                REJECTED,
                "blocked",
                blocker.message.clone(),
                json!({ "blocker": blocker }),
            ));
        }
        let Some(inventory) = snapshot.inventory.as_ref() else {
            return Err(ApiError::rejected(
                "unavailable",
                "could not discover MCUs from Moonraker",
            ));
        };
        let entries: Vec<_> = match &request {
            UpdateRequest::All { all: true } => body
                .mcus
                .iter()
                .filter(|m| m.actions.contains(&Action::Update))
                .collect(),
            UpdateRequest::All { all: false } => {
                return Err(ApiError::new(
                    INVALID_PARAMS,
                    "invalid_request",
                    "expected {\"all\": true} or {\"mcus\": [...]}",
                    Value::Null,
                ));
            }
            UpdateRequest::Mcus { mcus } => {
                let mut entries = Vec::new();
                for name in mcus {
                    let entry = body
                        .mcus
                        .iter()
                        .find(|m| m.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            ApiError::new(
                                REJECTED,
                                "unknown_mcu",
                                format!("no MCU named {name:?}"),
                                json!({ "mcu": name }),
                            )
                        })?;
                    entries.push(entry);
                }
                let blocked: Vec<_> = entries
                    .iter()
                    .filter(|m| !m.actions.contains(&Action::Update))
                    .map(|m| json!({ "name": m.name, "state": m.state, "message": m.message }))
                    .collect();
                if !blocked.is_empty() {
                    return Err(ApiError::new(
                        REJECTED,
                        "not_updatable",
                        "some requested MCUs cannot be updated",
                        json!({ "mcus": blocked }),
                    ));
                }
                entries
            }
        };
        if entries.is_empty() {
            return Err(ApiError::rejected(
                "nothing_to_update",
                "no MCU needs an update",
            ));
        }
        let objects: Vec<String> = entries
            .iter()
            .filter_map(|entry| {
                inventory
                    .mcus
                    .iter()
                    .find(|mcu| display_name(&mcu.name) == entry.name)
                    .map(|mcu| mcu.name.clone())
            })
            .collect();
        let targets = flash_order(inventory, &objects);
        let run_id = format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        );
        {
            let mut state = self.state.0.lock().unwrap();
            state.run = Some(RunBody {
                run_id: run_id.clone(),
                state: RunState::Running,
                messages: Vec::new(),
                result: None,
            });
            state.run_snapshot = Some(body);
        }
        let service = self.clone();
        let id = run_id.clone();
        std::thread::spawn(move || service.execute(&id, lock, &snapshot, &targets));
        Ok(json!({ "run_id": run_id }))
    }

    /// Blocks until no run is in progress or `timeout` elapses; returns whether it is idle.
    pub fn wait_until_idle(&self, timeout: Duration) -> bool {
        let (lock, condvar) = &*self.state;
        let state = lock.lock().unwrap();
        let (state, _) = condvar
            .wait_timeout_while(state, timeout, |state| {
                state
                    .run
                    .as_ref()
                    .is_some_and(|run| run.state == RunState::Running)
            })
            .unwrap();
        !state
            .run
            .as_ref()
            .is_some_and(|run| run.state == RunState::Running)
    }

    fn emit(&self, message: UpdateResponse) {
        if let Some(run) = self.state.0.lock().unwrap().run.as_mut() {
            push_capped(run, message.clone());
        }
        if let Some(sender) = self.events.lock().unwrap().as_ref() {
            let _ = sender.send(message);
        }
    }

    fn execute(&self, run_id: &str, lock: UpdateLock, snapshot: &Snapshot, targets: &[String]) {
        let names: Vec<_> = targets.iter().map(|target| display_name(target)).collect();
        self.emit(UpdateResponse {
            run_id: run_id.to_owned(),
            mcu: None,
            phase: Phase::Discover,
            message: format!("updating {}", names.join(", ")),
            complete: false,
            result: None,
        });
        let log = self.backend.run_log_path(run_id);
        let mut hooks = AgentHooks {
            service: self,
            run_id,
            mcu: None,
            phase: Phase::Discover,
        };
        let result = self.backend.run(&log, lock, snapshot, targets, &mut hooks);
        let (outcome, updated, failed) = match &result {
            Ok(outcome) => (Outcome::Success, outcome.updated.clone(), None),
            Err(failure) => (Outcome::Failed, failure.updated.clone(), Some(failure)),
        };
        let klipper_left_stopped = self.backend.klipper_active() == Some(false);
        let mcus = targets
            .iter()
            .map(|target| {
                let (outcome, message) = if updated.contains(target) {
                    (McuOutcome::Updated, "updated".to_owned())
                } else if failed.is_some_and(|f| f.mcu.as_deref() == Some(target.as_str())) {
                    (
                        McuOutcome::Failed,
                        failed.map(|f| f.message.clone()).unwrap_or_default(),
                    )
                } else {
                    (McuOutcome::NotAttempted, "not attempted".to_owned())
                };
                McuOutcomeBody {
                    name: display_name(target).to_owned(),
                    outcome,
                    message,
                }
            })
            .collect();
        let message = match failed {
            None => format!("{} updated; log: {}", updated.len(), log.display()),
            Some(failure) if klipper_left_stopped => format!(
                "{} Klipper was left stopped; recover {}, then start Klipper from the service menu; log: {}",
                failure.message,
                failure
                    .mcu
                    .as_deref()
                    .map(display_name)
                    .unwrap_or("the MCU"),
                log.display()
            ),
            Some(failure) => format!("{}; log: {}", failure.message, log.display()),
        };
        let result = RunResultBody {
            outcome,
            klippy_state: klippy_state_label(&self.backend.klippy_state()),
            klipper_left_stopped,
            mcus,
        };
        self.emit(UpdateResponse {
            run_id: run_id.to_owned(),
            mcu: None,
            phase: Phase::Done,
            message,
            complete: true,
            result: Some(result.clone()),
        });
        let (lock, condvar) = &*self.state;
        let mut state = lock.lock().unwrap();
        if let Some(run) = state.run.as_mut() {
            run.state = RunState::Finished;
            run.result = Some(result);
        }
        condvar.notify_all();
    }
}

struct AgentHooks<'a, B: AgentBackend> {
    service: &'a AgentService<B>,
    run_id: &'a str,
    mcu: Option<String>,
    phase: Phase,
}

impl<B: AgentBackend> AgentHooks<'_, B> {
    fn send(&mut self, phase: Phase, message: impl Into<String>) {
        self.phase = phase;
        self.service.emit(UpdateResponse {
            run_id: self.run_id.to_owned(),
            mcu: self.mcu.clone(),
            phase,
            message: message.into(),
            complete: false,
            result: None,
        });
    }
}

impl<B: AgentBackend> RunHooks for AgentHooks<'_, B> {
    fn approve(&mut self, name: &str, current: &str, next: &str) -> bool {
        self.mcu = Some(display_name(name).to_owned());
        self.send(Phase::Build, format!("updating from {current} to {next}"));
        true
    }

    fn progress(&mut self, _: &str, progress: UpdateProgress) {
        let (phase, message) = match progress {
            UpdateProgress::StoppingKlipper => (Phase::StopKlipper, "stopping Klipper"),
            UpdateProgress::ConfiguringFirmware => (Phase::Build, "configuring firmware"),
            UpdateProgress::CompilingFirmware => (Phase::Build, "compiling firmware"),
            UpdateProgress::EnteringBootloader => (Phase::EnterBootloader, "entering bootloader"),
            UpdateProgress::BootloaderReady => (Phase::EnterBootloader, "bootloader ready"),
            UpdateProgress::StartingFlash => (Phase::Flash, "flashing firmware"),
            UpdateProgress::Installing => (Phase::Flash, "installing host MCU"),
        };
        self.send(phase, message);
    }

    fn flashed(&mut self, _: &str, _: &str, padded_bytes: usize) {
        self.send(Phase::Flash, format!("wrote {padded_bytes} bytes"));
    }

    fn step_started(&mut self, step: RunStep) {
        match step {
            RunStep::WaitForApplication => {
                self.send(Phase::Verify, "waiting for the MCU to restart")
            }
            RunStep::StartKlipper => {
                self.mcu = None;
                self.send(Phase::StartKlipper, "starting Klipper");
            }
            RunStep::Reconnect => {
                self.send(Phase::Reconnect, "waiting for updated MCUs to reconnect")
            }
        }
    }

    fn step_succeeded(&mut self, step: RunStep) {
        match step {
            RunStep::WaitForApplication => self.send(Phase::Verify, "MCU restarted"),
            RunStep::StartKlipper => self.send(Phase::StartKlipper, "Klipper started"),
            RunStep::Reconnect => self.send(Phase::Reconnect, "all updated MCUs reconnected"),
        }
    }

    fn failed(&mut self, message: &str) {
        let phase = self.phase;
        self.send(phase, format!("error: {message}"));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::agent::api::{HostSnapshot, McuOutcome, Outcome, Phase};
    use crate::eligibility::CheckoutRevision;
    use crate::moonraker::{HostInfo, Mcu, McuInventory, McuTransport, PrintState};

    fn mcu(name: &str, version: &str, kconfig: &str) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: Some("Klipper".to_owned()),
            version: Some(version.to_owned()),
            mcu: "stm32".to_owned(),
            canbus_frequency_hz: None,
            transport: Some(McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 1,
            }),
            kconfig: kconfig.to_owned(),
        }
    }

    fn snapshot(print_state: PrintState) -> Snapshot {
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
                print_state: Some(print_state),
                config_error: false,
            },
            inventory: Some(McuInventory {
                mcus: vec![
                    mcu("mcu", "v1", "CONFIG_USBCANBUS=y\n"),
                    mcu("mcu RP2040", "v1", "CONFIG_MACH_RP2040=y\n"),
                    mcu("mcu current", "v2", "CONFIG_MACH_STM32=y\n"),
                ],
                unreported: Vec::new(),
            }),
        }
    }

    struct FakeBackend {
        snapshot: Snapshot,
        lock_dir: tempfile::TempDir,
        result: Mutex<Option<Result<RunOutcome, RunFailure>>>,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        seen_targets: Mutex<Vec<String>>,
        klipper_active: Option<bool>,
    }

    impl FakeBackend {
        fn new(snapshot: Snapshot, result: Result<RunOutcome, RunFailure>) -> Self {
            Self {
                snapshot,
                lock_dir: tempfile::tempdir().unwrap(),
                result: Mutex::new(Some(result)),
                gate: Mutex::new(None),
                seen_targets: Mutex::new(Vec::new()),
                klipper_active: Some(true),
            }
        }
    }

    impl AgentBackend for FakeBackend {
        fn snapshot(&self) -> Snapshot {
            self.snapshot.clone()
        }
        fn try_lock(&self) -> Result<UpdateLock, LockError> {
            UpdateLock::try_acquire(&self.lock_dir.path().join("lock"))
        }
        fn run(
            &self,
            _: &Path,
            _lock: UpdateLock,
            _: &Snapshot,
            targets: &[String],
            hooks: &mut dyn RunHooks,
        ) -> Result<RunOutcome, RunFailure> {
            *self.seen_targets.lock().unwrap() = targets.to_vec();
            for target in targets {
                hooks.approve(target, "v1", "v2");
            }
            if let Some(gate) = self.gate.lock().unwrap().take() {
                gate.recv().unwrap();
            }
            self.result.lock().unwrap().take().unwrap()
        }
        fn klipper_active(&self) -> Option<bool> {
            self.klipper_active
        }
        fn klippy_state(&self) -> KlippyState {
            KlippyState::Ready
        }
        fn run_log_path(&self, run_id: &str) -> PathBuf {
            PathBuf::from(format!("/tmp/{run_id}.log"))
        }
    }

    fn sink() -> (EventSink, mpsc::Receiver<UpdateResponse>) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Mutex::new(Some(tx))), rx)
    }

    fn reason(error: &ApiError) -> &str {
        error.data["reason"].as_str().unwrap()
    }

    #[test]
    fn updates_all_updatable_mcus_with_the_bridge_last() {
        let backend = FakeBackend::new(
            snapshot(PrintState::Standby),
            Ok(RunOutcome {
                updated: vec!["mcu RP2040".to_owned(), "mcu".to_owned()],
            }),
        );
        let (events, rx) = sink();
        let service = AgentService::new(backend, events);

        let accepted = service.update(UpdateRequest::All { all: true }).unwrap();
        assert!(accepted["run_id"].is_string());
        assert!(service.wait_until_idle(Duration::from_secs(5)));

        assert_eq!(
            *service.backend.seen_targets.lock().unwrap(),
            vec!["mcu RP2040", "mcu"]
        );
        let run = service.status().run.unwrap();
        let result = run.result.unwrap();
        assert_eq!(result.outcome, Outcome::Success);
        assert!(!result.klipper_left_stopped);
        assert!(result.mcus.iter().all(|m| m.outcome == McuOutcome::Updated));
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.first().unwrap().phase, Phase::Discover);
        assert_eq!(events.first().unwrap().message, "updating RP2040, mcu");
        assert_eq!(events.last().unwrap().phase, Phase::Done);
        assert!(events.last().unwrap().complete);
        assert_eq!(run.messages.last(), events.last());
    }

    #[test]
    fn resolves_display_names_case_insensitively() {
        let backend = FakeBackend::new(
            snapshot(PrintState::Standby),
            Ok(RunOutcome {
                updated: vec!["mcu RP2040".to_owned()],
            }),
        );
        let service = AgentService::new(backend, sink().0);

        service
            .update(UpdateRequest::Mcus {
                mcus: vec!["rp2040".to_owned()],
            })
            .unwrap();
        assert!(service.wait_until_idle(Duration::from_secs(5)));

        assert_eq!(
            *service.backend.seen_targets.lock().unwrap(),
            vec!["mcu RP2040"]
        );
    }

    #[test]
    fn rejects_whole_requests_with_documented_reasons() {
        let backend = FakeBackend::new(snapshot(PrintState::Standby), Ok(RunOutcome::default()));
        let service = AgentService::new(backend, sink().0);

        let error = service
            .update(UpdateRequest::Mcus {
                mcus: vec!["rp2040".to_owned(), "nope".to_owned()],
            })
            .unwrap_err();
        assert_eq!(reason(&error), "unknown_mcu");
        let error = service
            .update(UpdateRequest::Mcus {
                mcus: vec!["rp2040".to_owned(), "current".to_owned()],
            })
            .unwrap_err();
        assert_eq!(reason(&error), "not_updatable");
        assert_eq!(error.data["mcus"][0]["name"], "current");
        let error = service
            .update(UpdateRequest::All { all: false })
            .unwrap_err();
        assert_eq!(reason(&error), "invalid_request");
        assert!(service.backend.seen_targets.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_when_blocked_or_busy() {
        let backend = FakeBackend::new(snapshot(PrintState::Printing), Ok(RunOutcome::default()));
        let service = AgentService::new(backend, sink().0);
        let error = service
            .update(UpdateRequest::All { all: true })
            .unwrap_err();
        assert_eq!(reason(&error), "blocked");
        assert_eq!(error.data["blocker"]["reason"], "printing");

        let backend = FakeBackend::new(snapshot(PrintState::Standby), Ok(RunOutcome::default()));
        let _held = backend.try_lock().unwrap();
        let service = AgentService::new(backend, sink().0);
        assert_eq!(
            reason(
                &service
                    .update(UpdateRequest::All { all: true })
                    .unwrap_err()
            ),
            "busy"
        );
    }

    #[test]
    fn reports_a_failed_run_that_left_klipper_stopped() {
        let mut backend = FakeBackend::new(
            snapshot(PrintState::Standby),
            Err(RunFailure {
                message: "flash failed".to_owned(),
                mcu: Some("mcu RP2040".to_owned()),
                updated: Vec::new(),
            }),
        );
        backend.klipper_active = Some(false);
        let service = AgentService::new(backend, sink().0);

        service.update(UpdateRequest::All { all: true }).unwrap();
        assert!(service.wait_until_idle(Duration::from_secs(5)));

        let result = service.status().run.unwrap().result.unwrap();
        assert_eq!(result.outcome, Outcome::Failed);
        assert!(result.klipper_left_stopped);
        let outcomes: Vec<_> = result
            .mcus
            .iter()
            .map(|m| (m.name.as_str(), m.outcome))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("RP2040", McuOutcome::Failed),
                ("mcu", McuOutcome::NotAttempted)
            ]
        );
    }

    #[test]
    fn serves_the_run_snapshot_without_actions_during_a_run() {
        let backend = FakeBackend::new(
            snapshot(PrintState::Standby),
            Ok(RunOutcome {
                updated: vec!["mcu".to_owned()],
            }),
        );
        let (release, gate) = mpsc::channel();
        *backend.gate.lock().unwrap() = Some(gate);
        let service = AgentService::new(backend, sink().0);

        service
            .update(UpdateRequest::Mcus {
                mcus: vec!["mcu".to_owned()],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let during = service.status();
        release.send(()).unwrap();
        assert!(service.wait_until_idle(Duration::from_secs(5)));

        let run = during.run.unwrap();
        assert!(matches!(run.state, RunState::Running));
        assert!(!run.messages.is_empty());
        assert!(during.mcus.iter().all(|m| m.actions.is_empty()));
    }

    #[test]
    fn caps_history_but_keeps_the_final_message() {
        let mut record = RunBody {
            run_id: "r".to_owned(),
            state: RunState::Running,
            messages: Vec::new(),
            result: None,
        };
        for index in 0..600 {
            push_capped(
                &mut record,
                UpdateResponse {
                    run_id: "r".to_owned(),
                    mcu: None,
                    phase: Phase::Build,
                    message: index.to_string(),
                    complete: false,
                    result: None,
                },
            );
        }
        assert_eq!(record.messages.len(), MESSAGE_CAP);
        assert_eq!(record.messages.last().unwrap().message, "599");
    }
}
