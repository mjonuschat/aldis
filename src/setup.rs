//! Host permission setup: udev rules and the sudoers policy for the Klipper service.

use std::fs;
use std::process::{Command, ExitCode};

use anyhow::{Context, bail};

use crate::cli::SetupArgs;
use crate::fail;

const UDEV_RULES_PATH: &str = "/etc/udev/rules.d/80-aldis.rules";
const SUDOERS_PATH: &str = "/etc/sudoers.d/aldis";
const UDEV_RULES: &str = include_str!("../templates/80-aldis.rules");

pub(crate) fn setup(arguments: SetupArgs) -> ExitCode {
    if arguments.check {
        check_setup()
    } else {
        match install_setup() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(format!("{error:#}")),
        }
    }
}

fn install_setup() -> anyhow::Result<()> {
    let Some(user) = std::env::var_os("SUDO_USER").and_then(|user| user.into_string().ok()) else {
        bail!("setup must be run with sudo; run: sudo aldis setup");
    };
    if !valid_user_name(&user) {
        bail!("SUDO_USER is not a valid account name");
    }
    let rules_action = install_file(UDEV_RULES_PATH, udev_rules(), "udev rules")?;
    let service_policy = sudoers_policy(&user);
    let service_action = install_file(SUDOERS_PATH, &service_policy, "service policy")?;
    let status = Command::new("chmod")
        .args(["440", SUDOERS_PATH])
        .status()
        .context("could not protect service policy")?;
    if !status.success() {
        bail!("could not protect service policy: chmod exited with {status}");
    }
    let status = Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
        .context("could not reload udev rules")?;
    if !status.success() {
        bail!("could not reload udev rules: udevadm exited with {status}");
    }
    println!("{}", setup_install_report(rules_action, service_action));
    Ok(())
}

fn install_file(path: &str, contents: &str, description: &str) -> anyhow::Result<&'static str> {
    if fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok("already current");
    }
    fs::write(path, contents).with_context(|| format!("could not install {description}"))?;
    Ok("installed")
}

fn setup_install_report(rules_action: &str, service_action: &str) -> String {
    format!(
        "aldis setup:\n  udev rules: {rules_action}\n  service policy: {service_action}\n  service policy permissions: set to 0440\n  udev rules: reloaded"
    )
}

fn check_setup() -> ExitCode {
    let rules = fs::read_to_string(UDEV_RULES_PATH).is_ok_and(|contents| contents == udev_rules());
    let service = Command::new("sudo")
        .args(["-n", "/bin/systemctl", "is-active", "klipper"])
        .output()
        .is_ok_and(|output| sudo_policy_allows_service_status(&output.stdout));
    println!("{}", setup_check_report(rules, service));
    if rules && service {
        ExitCode::SUCCESS
    } else {
        fail("aldis setup is incomplete; run sudo aldis setup".to_owned())
    }
}

fn setup_check_report(rules: bool, service: bool) -> String {
    format!(
        "aldis setup:\n  udev rules: {}\n  Klipper service access: {}",
        if rules {
            "ready"
        } else {
            "missing or outdated"
        },
        if service { "ready" } else { "unavailable" },
    )
}

fn sudo_policy_allows_service_status(stdout: &[u8]) -> bool {
    matches!(
        std::str::from_utf8(stdout).map(str::trim),
        Ok("active" | "inactive" | "failed" | "activating" | "deactivating" | "reloading")
    )
}

fn valid_user_name(user: &str) -> bool {
    !user.is_empty()
        && user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn udev_rules() -> &'static str {
    UDEV_RULES
}

fn sudoers_policy(user: &str) -> String {
    format!(
        "{user} ALL=(root) NOPASSWD: /bin/systemctl is-active klipper, /bin/systemctl stop klipper, /bin/systemctl start klipper\n"
    )
}

#[cfg(test)]
mod tests {
    use super::{setup_check_report, setup_install_report, sudo_policy_allows_service_status};

    #[test]
    fn recognizes_a_permitted_inactive_klipper_status() {
        assert!(sudo_policy_allows_service_status(b"inactive\n"));
        assert!(!sudo_policy_allows_service_status(
            b"sudo: a password is required\n"
        ));
    }

    #[test]
    fn reports_each_setup_prerequisite() {
        assert_eq!(
            setup_check_report(true, false),
            "aldis setup:\n  udev rules: ready\n  Klipper service access: unavailable"
        );
    }

    #[test]
    fn reports_setup_install_actions() {
        assert_eq!(
            setup_install_report("already current", "installed"),
            "aldis setup:\n  udev rules: already current\n  service policy: installed\n  service policy permissions: set to 0440\n  udev rules: reloaded"
        );
    }
}
