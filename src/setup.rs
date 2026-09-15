//! Host permission setup: udev rules and the sudoers policy for the Klipper service.

use std::fs;
use std::process::{Command, ExitCode};

use crate::cli::SetupArgs;
use crate::fail;

const UDEV_RULES_PATH: &str = "/etc/udev/rules.d/80-mcu-update.rules";
const SUDOERS_PATH: &str = "/etc/sudoers.d/mcu-update";
const UDEV_RULES: &str = include_str!("../templates/80-mcu-update.rules");

pub(crate) fn setup(arguments: SetupArgs) -> ExitCode {
    if arguments.check {
        check_setup()
    } else {
        install_setup()
    }
}

fn install_setup() -> ExitCode {
    let Some(user) = std::env::var_os("SUDO_USER").and_then(|user| user.into_string().ok()) else {
        return fail("setup must be run with sudo; run: sudo mcu-update setup".to_owned());
    };
    if !valid_user_name(&user) {
        return fail("SUDO_USER is not a valid account name".to_owned());
    }
    let rules_action = match install_file(UDEV_RULES_PATH, udev_rules(), "udev rules") {
        Ok(action) => action,
        Err(error) => return fail(error),
    };
    let service_policy = sudoers_policy(&user);
    let service_action = match install_file(SUDOERS_PATH, &service_policy, "service policy") {
        Ok(action) => action,
        Err(error) => return fail(error),
    };
    match Command::new("chmod").args(["440", SUDOERS_PATH]).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return fail(format!(
                "could not protect service policy: chmod exited with {status}"
            ));
        }
        Err(error) => return fail(format!("could not protect service policy: {error}")),
    }
    match Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return fail(format!(
                "could not reload udev rules: udevadm exited with {status}"
            ));
        }
        Err(error) => return fail(format!("could not reload udev rules: {error}")),
    }
    println!("{}", setup_install_report(rules_action, service_action));
    ExitCode::SUCCESS
}

fn install_file(path: &str, contents: &str, description: &str) -> Result<&'static str, String> {
    if fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok("already current");
    }
    fs::write(path, contents)
        .map_err(|error| format!("could not install {description}: {error}"))?;
    Ok("installed")
}

fn setup_install_report(rules_action: &str, service_action: &str) -> String {
    format!(
        "mcu-update setup:\n  udev rules: {rules_action}\n  service policy: {service_action}\n  service policy permissions: set to 0440\n  udev rules: reloaded"
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
        fail("mcu-update setup is incomplete; run sudo mcu-update setup".to_owned())
    }
}

fn setup_check_report(rules: bool, service: bool) -> String {
    format!(
        "mcu-update setup:\n  udev rules: {}\n  Klipper service access: {}",
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
            "mcu-update setup:\n  udev rules: ready\n  Klipper service access: unavailable"
        );
    }

    #[test]
    fn reports_setup_install_actions() {
        assert_eq!(
            setup_install_report("already current", "installed"),
            "mcu-update setup:\n  udev rules: already current\n  service policy: installed\n  service policy permissions: set to 0440\n  udev rules: reloaded"
        );
    }
}
