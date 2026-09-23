use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::logging::LoggingCommandAdapter;
use aldis::self_update::{
    GithubReleaseAdapter, ReleasePort, extract_binary, install_binary, is_up_to_date,
    target_platform,
};

use crate::cli::SelfUpdateArgs;
use crate::fail;
use crate::ui::UpdateUi;

const REPO: &str = "mjonuschat/aldis";

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
        .download_archive(platform)
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
    ui.action("run completed successfully");
    Ok(())
}
