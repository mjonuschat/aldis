use std::process::ExitCode;

use anyhow::Context;

use aldis::build::{BuildCommand, CommandPort, SystemCommandAdapter};
use aldis::logging::LoggingCommandAdapter;
use aldis::self_update::{
    GithubReleaseAdapter, ReleasePort, extract_binary, install_binary, is_up_to_date,
    target_platform,
};

use crate::cli::SelfUpdateArgs;
use crate::fail;
use crate::setup::{AGENT_RESTART_COMMAND, AGENT_STATUS_COMMAND, AGENT_UNIT_PATH};
use crate::ui::UpdateUi;

const REPO: &str = "mjonuschat/aldis";

/// What happened when `self-update` tried to restart the agent onto the new binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartOutcome {
    /// The agent was running and has been restarted.
    Restarted,
    /// The agent was not running, so it was left alone.
    NotRunning,
    /// The agent is running but could not be restarted, e.g. the sudoers grant
    /// this needs is missing until `aldis setup --agent` is re-run.
    CouldNotRestart,
}

/// Restarts the `aldis` agent through the NOPASSWD sudoers grant `aldis setup
/// --agent` installs, but only if it is currently active.
fn restart_agent_if_running(runner: &impl CommandPort) -> RestartOutcome {
    let is_active = match runner.run(&sudo(&AGENT_STATUS_COMMAND)) {
        Ok(output) => match std::str::from_utf8(&output.stdout).map(str::trim) {
            Ok("active") => true,
            Ok("inactive" | "failed" | "activating" | "deactivating" | "reloading") => false,
            // Anything else — a missing sudoers grant, a sudo policy denial, a
            // systemd connection error — means we can't tell, so don't guess.
            _ => return RestartOutcome::CouldNotRestart,
        },
        Err(_) => return RestartOutcome::CouldNotRestart,
    };
    if !is_active {
        return RestartOutcome::NotRunning;
    }
    match runner.run(&sudo(&AGENT_RESTART_COMMAND)) {
        Ok(output) if output.success => RestartOutcome::Restarted,
        _ => RestartOutcome::CouldNotRestart,
    }
}

fn sudo(arguments: &[&str]) -> BuildCommand {
    BuildCommand {
        program: "sudo".to_owned(),
        arguments: std::iter::once("-n")
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect(),
        current_dir: None,
        stdin: None,
    }
}

pub(crate) fn self_update(arguments: SelfUpdateArgs, mut ui: UpdateUi) -> ExitCode {
    match run_self_update(arguments, &mut ui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(format!("{error:#}")),
    }
}

fn run_self_update(arguments: SelfUpdateArgs, ui: &mut UpdateUi) -> anyhow::Result<()> {
    let adapter = GithubReleaseAdapter::new(REPO);
    let current = env!("CARGO_PKG_VERSION");
    ui.action("checking for a newer aldis release");
    let latest = adapter
        .latest_release()
        .context("failed to check for a newer release")?;

    if is_up_to_date(current, &latest.tag) {
        ui.block(&format!("aldis {current} is already up to date\n"));
        ui.action("run completed successfully: already up to date");
        return Ok(());
    }
    if arguments.check {
        ui.block(&format!(
            "a newer release is available: {current} -> {}\n",
            latest.tag
        ));
        ui.action("run completed successfully: update available");
        return Ok(());
    }

    let platform =
        target_platform(std::env::consts::ARCH).context("cannot self-update on this platform")?;
    ui.begin(format!("downloading {}", latest.tag));
    let archive_bytes = adapter
        .download_archive(&latest.tag, platform)
        .context("failed to download the release archive")?;
    ui.finish_success(format!("downloaded {} bytes", archive_bytes.len()));

    let workspace = tempfile::Builder::new()
        .prefix("aldis-self-update-")
        .tempdir()
        .context("could not create a temporary directory")?;
    let archive_path = workspace.path().join("aldis.tar.xz");
    std::fs::write(&archive_path, &archive_bytes)
        .context("could not write the downloaded archive")?;

    ui.begin("extracting");
    let runner = LoggingCommandAdapter::new(SystemCommandAdapter);
    let extracted = extract_binary(&runner, &archive_path, workspace.path())
        .context("failed to extract the downloaded archive")?;
    ui.finish_success("extracted");

    let current_exe =
        std::env::current_exe().context("could not determine the running executable's path")?;
    ui.begin("installing");
    install_binary(&extracted, &current_exe).context("failed to install the new binary")?;
    ui.finish_success(format!("installed aldis {}", latest.tag));
    if std::path::Path::new(AGENT_UNIT_PATH).exists() {
        match restart_agent_if_running(&runner) {
            RestartOutcome::Restarted => {
                ui.block("restarted the aldis agent onto the new version\n")
            }
            RestartOutcome::NotRunning => {}
            RestartOutcome::CouldNotRestart => ui.block(
                "the aldis agent keeps running the previous version until restarted: \
                 use Fluidd's service menu or run `sudo systemctl restart aldis`\n",
            ),
        }
    }
    ui.action("run completed successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use aldis::build::{CommandError, CommandOutput};

    use super::{RestartOutcome, restart_agent_if_running};

    struct FakeRunner(RefCell<VecDeque<CommandOutput>>);

    impl FakeRunner {
        fn new(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self(RefCell::new(outputs.into_iter().collect()))
        }
    }

    impl aldis::build::CommandPort for FakeRunner {
        fn run(
            &self,
            _command: &aldis::build::BuildCommand,
        ) -> Result<CommandOutput, CommandError> {
            self.0
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| CommandError::Spawn(std::io::Error::other("missing fake output")))
        }
    }

    fn state_output(success: bool, stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn restarts_a_running_agent() {
        let runner = FakeRunner::new([state_output(true, b"active\n"), CommandOutput::success()]);
        assert_eq!(restart_agent_if_running(&runner), RestartOutcome::Restarted);
    }

    #[test]
    fn leaves_a_stopped_agent_alone() {
        let runner = FakeRunner::new([state_output(false, b"inactive\n")]);
        assert_eq!(
            restart_agent_if_running(&runner),
            RestartOutcome::NotRunning
        );
    }

    #[test]
    fn reports_could_not_restart_when_the_sudoers_grant_is_missing() {
        let runner = FakeRunner::new([CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"sudo: a password is required\n".to_vec(),
        }]);
        assert_eq!(
            restart_agent_if_running(&runner),
            RestartOutcome::CouldNotRestart
        );
    }

    #[test]
    fn reports_could_not_restart_on_an_unrecognized_status_check_result() {
        let runner = FakeRunner::new([CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"sudo: aldis-status: command not allowed\n".to_vec(),
        }]);
        assert_eq!(
            restart_agent_if_running(&runner),
            RestartOutcome::CouldNotRestart
        );
    }

    #[test]
    fn reports_could_not_restart_when_the_restart_command_itself_fails() {
        let runner = FakeRunner::new([state_output(true, b"active\n"), state_output(false, b"")]);
        assert_eq!(
            restart_agent_if_running(&runner),
            RestartOutcome::CouldNotRestart
        );
    }
}
