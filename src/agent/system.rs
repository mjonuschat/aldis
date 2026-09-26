//! The real [`AgentBackend`]: Moonraker over HTTP, the local checkout, systemd, and MCUs.

use std::path::{Path, PathBuf};

use crate::agent::api::{HostSnapshot, Snapshot};
use crate::agent::service::AgentBackend;
use crate::build::SystemCommandAdapter;
use crate::checkout;
use crate::coordinator::BuildCoordinator;
use crate::eligibility::CheckoutRevision;
use crate::host::{check_host, is_loopback_url};
use crate::identify::{SerialIdentify, resolve_unreported, should_probe};
use crate::lock::{LockError, UpdateLock};
use crate::logging::{LoggingCommandAdapter, RunLogSink};
use crate::moonraker::{
    HostInfo, HostPort, KlippyState, MoonrakerAdapter, MoonrakerError, PrinterStatePort,
};
use crate::service::{KlipperService, ServiceState};
use crate::update_run::{RunFailure, RunHooks, RunOutcome, RunRequest, run_updates};
use crate::workspace::RunWorkspace;

pub struct SystemBackend {
    url: String,
    log_sink: RunLogSink,
}

impl SystemBackend {
    pub fn new(url: String, log_sink: RunLogSink) -> Self {
        Self { url, log_sink }
    }

    fn adapter(&self) -> MoonrakerAdapter {
        MoonrakerAdapter::new(&self.url)
    }
}

/// `<logs root>/aldis/<run_id>.log`, falling back to the temp directory when Moonraker reports no
/// logs root or the log file cannot be created there. Creates the file, so the path returned is
/// one the run can actually write.
pub fn run_log_path_in(logs_root: Option<&Path>, run_id: &str) -> PathBuf {
    let creatable = |path: &Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .is_ok()
    };
    if let Some(root) = logs_root {
        let dir = root.join("aldis");
        let path = dir.join(format!("{run_id}.log"));
        if std::fs::create_dir_all(&dir).is_ok() && creatable(&path) {
            return path;
        }
    }
    std::env::temp_dir().join(format!("aldis-{run_id}.log"))
}

impl AgentBackend for SystemBackend {
    fn snapshot(&self) -> Snapshot {
        let adapter = self.adapter();
        let url_is_local = is_loopback_url(&self.url);
        let unavailable = |message: String| HostInfo {
            state: KlippyState::Disconnected,
            state_message: message,
            software_version: None,
            klipper_path: None,
        };
        let (klipper_unit, unit_error) = match url_is_local.then(|| adapter.klipper_unit()) {
            Some(Ok(unit)) => (unit, None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        // Like `verify_host`: an instance that cannot be identified is not updatable, so report
        // Klippy as unavailable rather than assuming the default unit.
        let info = match unit_error {
            Some(error) => unavailable(format!(
                "could not ask Moonraker which Klipper service it manages: {}",
                crate::error_chain(&error)
            )),
            None => adapter
                .host_info()
                .unwrap_or_else(|error| unavailable(crate::error_chain(&error))),
        };
        let mut snapshot = Snapshot {
            host: HostSnapshot {
                url_is_local,
                klipper_unit,
                info,
                checkout_version: None,
                print_state: None,
                config_error: false,
            },
            inventory: None,
        };
        if snapshot.host.info.state == KlippyState::Disconnected
            || check_host(&self.url, snapshot.host.klipper_unit.as_deref()).is_err()
        {
            return snapshot;
        }
        match adapter.discover_mcus() {
            Ok(mut inventory) => {
                if should_probe(&snapshot.host.info.state) {
                    resolve_unreported(&mut inventory, &SerialIdentify::default());
                }
                snapshot.inventory = Some(inventory);
            }
            Err(MoonrakerError::ConfigError(_)) => snapshot.host.config_error = true,
            Err(error) => tracing::debug!(?error, "discovery failed"),
        }
        snapshot.host.checkout_version = snapshot
            .host
            .info
            .klipper_path
            .as_deref()
            .map(|path| checkout::revision(path).unwrap_or(CheckoutRevision::Indeterminate));
        if snapshot.host.info.state == KlippyState::Ready {
            snapshot.host.print_state = adapter.print_state().ok();
        }
        snapshot
    }

    fn try_lock(&self) -> Result<UpdateLock, LockError> {
        UpdateLock::try_acquire_default()
    }

    fn run(
        &self,
        log: &Path,
        lock: UpdateLock,
        snapshot: &Snapshot,
        targets: &[String],
        hooks: &mut dyn RunHooks,
    ) -> Result<RunOutcome, RunFailure> {
        let _lock = lock;
        if let Err(error) = self.log_sink.start(log) {
            tracing::warn!(?error, path = %log.display(), "could not open the run log");
        }
        let result = self.run_logged(snapshot, targets, hooks);
        self.log_sink.stop();
        result
    }

    fn klipper_active(&self) -> Option<bool> {
        KlipperService::new(SystemCommandAdapter)
            .state()
            .ok()
            .map(|state| state == ServiceState::Active)
    }

    fn klippy_state(&self) -> KlippyState {
        self.adapter()
            .host_info()
            .map_or(KlippyState::Disconnected, |info| info.state)
    }

    fn run_log_path(&self, run_id: &str) -> PathBuf {
        run_log_path_in(self.adapter().logs_root().ok().flatten().as_deref(), run_id)
    }
}

impl SystemBackend {
    fn run_logged(
        &self,
        snapshot: &Snapshot,
        targets: &[String],
        hooks: &mut dyn RunHooks,
    ) -> Result<RunOutcome, RunFailure> {
        let setup_failure = |message: String| RunFailure {
            message,
            mcu: None,
            updated: Vec::new(),
        };
        let (Some(inventory), Some(source), Some(version)) = (
            snapshot.inventory.as_ref(),
            snapshot.host.info.klipper_path.as_ref(),
            snapshot.host.info.software_version.as_ref(),
        ) else {
            return Err(setup_failure(
                "Moonraker did not report the Klipper version and path".to_owned(),
            ));
        };
        let workspace = tempfile::Builder::new()
            .prefix("aldis-")
            .tempdir()
            .map(|dir| RunWorkspace::adopt(dir.keep()))
            .map_err(|error| {
                setup_failure(format!("could not create the run workspace: {error}"))
            })?;
        let coordinator = BuildCoordinator::new(
            source,
            LoggingCommandAdapter::new(SystemCommandAdapter),
            LoggingCommandAdapter::new(SystemCommandAdapter),
        );
        run_updates(
            &coordinator,
            &self.adapter(),
            inventory,
            &workspace,
            RunRequest {
                targets,
                target_revision: &CheckoutRevision::Known(version.clone()),
                clean: false,
            },
            hooks,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::run_log_path_in;

    #[test]
    fn logs_under_moonrakers_logs_root_or_the_temp_dir() {
        let root = tempfile::tempdir().unwrap();
        let path = run_log_path_in(Some(root.path()), "123");
        assert_eq!(path, root.path().join("aldis").join("123.log"));
        assert!(root.path().join("aldis").is_dir());

        let missing = std::path::Path::new("/proc/definitely-not-writable");
        assert_eq!(
            run_log_path_in(Some(missing), "123"),
            std::env::temp_dir().join("aldis-123.log")
        );
        assert_eq!(
            run_log_path_in(None, "123"),
            std::env::temp_dir().join("aldis-123.log")
        );

        // An existing aldis directory the agent cannot write into (e.g. left behind by root).
        let locked = tempfile::tempdir().unwrap();
        std::fs::create_dir(locked.path().join("aldis")).unwrap();
        let blocked = locked.path().join("aldis").join("456.log");
        std::fs::create_dir(&blocked).unwrap();
        assert_eq!(
            run_log_path_in(Some(locked.path()), "456"),
            std::env::temp_dir().join("aldis-456.log")
        );
    }
}
