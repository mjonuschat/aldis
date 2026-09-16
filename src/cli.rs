//! Command-line argument parsing.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

/// Safely update Klipper MCU firmware from its embedded configuration.
#[derive(Debug, Parser)]
#[command(name = "aldis", version, about)]
pub(crate) struct Cli {
    /// When to use ANSI color in interactive update output.
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    pub(crate) color: ColorMode,
    /// Disable animated progress indicators.
    #[arg(long, global = true)]
    pub(crate) no_progress: bool,
    #[command(subcommand)]
    pub(crate) command: CliCommand,
}

/// Controls ANSI color in interactive output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum ColorMode {
    /// Use color only when stderr is a terminal and NO_COLOR is not set.
    #[default]
    Auto,
    /// Always emit ANSI color sequences.
    Always,
    /// Never emit ANSI color sequences.
    Never,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CliCommand {
    /// Show discovered MCUs and whether their running firmware is current.
    Status(ConnectionArgs),
    /// Show the MCU configuration reported by Moonraker.
    Inspect(MoonrakerArgs),
    /// Build and flash one MCU or every eligible MCU.
    Update(UpdateArgs),
    /// Install or verify the host permissions required for unprivileged updates.
    Setup(SetupArgs),
}

#[derive(Debug, Args)]
pub(crate) struct MoonrakerArgs {
    /// Moonraker API URL.
    #[arg(long, default_value = DEFAULT_MOONRAKER_URL)]
    pub(crate) moonraker: String,
}

#[derive(Debug, Args)]
pub(crate) struct ConnectionArgs {
    #[command(flatten)]
    pub(crate) moonraker: MoonrakerArgs,
    /// Klipper source checkout.
    #[arg(long)]
    pub(crate) klipper_source: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .args(["targets", "all", "auto"])
))]
pub(crate) struct UpdateArgs {
    /// One or more MCU names reported by Moonraker.
    #[arg(value_name = "MCU", num_args = 1.., conflicts_with_all = ["all", "auto"])]
    pub(crate) targets: Vec<String>,
    /// Update every eligible MCU.
    #[arg(long, conflicts_with_all = ["targets", "auto"])]
    pub(crate) all: bool,
    /// Fast-forward, then update every outdated supported MCU without prompts.
    #[arg(long, conflicts_with_all = ["targets", "all", "force", "pull"])]
    pub(crate) auto: bool,
    /// Update even when the MCU already reports the checkout revision.
    #[arg(long)]
    pub(crate) force: bool,
    /// Fast-forward the configured Klipper checkout before assessing MCUs.
    #[arg(long)]
    pub(crate) pull: bool,
    /// Discard Klipper's existing build output before compiling each target.
    #[arg(long)]
    pub(crate) clean: bool,
    #[command(flatten)]
    pub(crate) connection: ConnectionArgs,
    /// Directory retained for generated Kconfigs and firmware artifacts.
    #[arg(long)]
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct SetupArgs {
    /// Verify installed host permissions without modifying them.
    #[arg(long)]
    pub(crate) check: bool,
}

#[cfg(test)]
mod tests {
    use super::{Cli, CliCommand};
    use clap::{CommandFactory, Parser};

    #[test]
    fn top_level_help_describes_the_cli() {
        let help = Cli::command().render_help().to_string();

        assert!(help.contains("Usage:"));
        assert!(help.contains("setup"));
        assert!(help.contains("update"));
    }

    #[test]
    fn requires_exactly_one_update_target_selector() {
        assert!(Cli::try_parse_from(["aldis", "update"]).is_err());
        assert!(Cli::try_parse_from(["aldis", "update", "mcu", "--all"]).is_err());
        assert!(Cli::try_parse_from(["aldis", "update", "--all"]).is_ok());
    }

    #[test]
    fn accepts_multiple_mcu_targets_or_noninteractive_auto_updates() {
        let CliCommand::Update(targeted) =
            Cli::try_parse_from(["aldis", "update", "mcu", "mcu toolhead"])
                .expect("parse multiple targets")
                .command
        else {
            panic!("expected update command");
        };
        assert_eq!(targeted.targets, ["mcu", "mcu toolhead"]);

        let CliCommand::Update(automatic) = Cli::try_parse_from(["aldis", "update", "--auto"])
            .expect("parse automatic update")
            .command
        else {
            panic!("expected update command");
        };
        assert!(automatic.auto);
        assert!(Cli::try_parse_from(["aldis", "update", "--auto", "--pull"]).is_err());
    }

    #[test]
    fn accepts_a_clean_flag_alongside_other_selectors() {
        let CliCommand::Update(cleaned) =
            Cli::try_parse_from(["aldis", "update", "--all", "--clean"])
                .expect("parse clean update")
                .command
        else {
            panic!("expected update command");
        };
        assert!(cleaned.clean);
    }
}
