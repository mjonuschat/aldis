//! Host permission setup: udev rules and the sudoers policy for the Klipper service.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    tracing::info!("installing udev rules");
    let rules_action = install_file(UDEV_RULES_PATH, udev_rules(), "udev rules")?;
    tracing::debug!(rules_action, "udev rules installed");
    tracing::info!("installing service policy");
    let service_policy = sudoers_policy(&user);
    let service_action = install_sudoers_policy(SUDOERS_PATH, &service_policy)?;
    tracing::debug!(service_action, "service policy installed");
    let status = Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
        .context("could not reload udev rules")?;
    if !status.success() {
        bail!("could not reload udev rules: udevadm exited with {status}");
    }
    let status = Command::new("udevadm")
        .args(["trigger", "--subsystem-match=usb", "--settle"])
        .status()
        .context("could not trigger udev rules")?;
    if !status.success() {
        bail!("could not trigger udev rules: udevadm exited with {status}");
    }
    tracing::info!("udev rules reloaded and triggered");
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

fn install_sudoers_policy(path: &str, contents: &str) -> anyhow::Result<&'static str> {
    let action = if fs::read_to_string(path).is_ok_and(|current| current == contents) {
        "already current"
    } else {
        stage_and_install_sudoers(path, contents)?;
        "installed"
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o440))
        .context("could not protect service policy")?;
    Ok(action)
}

fn stage_and_install_sudoers(path: &str, contents: &str) -> anyhow::Result<()> {
    let dir = Path::new(path)
        .parent()
        .context("service policy path has no parent directory")?;
    let staged = stage_sudoers(dir, contents)?;

    let status = Command::new("visudo")
        .args(["-c", "-f"])
        .arg(staged.path())
        .status()
        .context("could not validate the service policy with visudo")?;
    if !status.success() {
        bail!("staged service policy failed visudo validation: visudo exited with {status}");
    }

    staged
        .persist(path)
        .context("could not install the validated service policy")?;
    Ok(())
}

fn stage_sudoers(dir: &Path, contents: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut staged = tempfile::Builder::new()
        .prefix(".aldis-sudoers-")
        .tempfile_in(dir)
        .context("could not stage the service policy")?;
    staged
        .write_all(contents.as_bytes())
        .context("could not write the staged service policy")?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o440))
        .context("could not protect the staged service policy")?;
    Ok(staged)
}

fn setup_install_report(rules_action: &str, service_action: &str) -> String {
    format!(
        "aldis setup:\n  udev rules: {rules_action}\n  service policy: {service_action}\n  service policy permissions: set to 0440\n  udev rules: reloaded and triggered"
    )
}

fn check_setup() -> ExitCode {
    tracing::info!("checking aldis setup");
    let rules = fs::read_to_string(UDEV_RULES_PATH).is_ok_and(|contents| contents == udev_rules());
    let service = Command::new("sudo")
        .args(["-n", "/bin/systemctl", "is-active", "klipper"])
        .output()
        .is_ok_and(|output| sudo_policy_allows_service_status(&output.stdout));
    tracing::debug!(rules, service, "setup check results");
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        setup_check_report, setup_install_report, stage_sudoers, sudo_policy_allows_service_status,
    };

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
            "aldis setup:\n  udev rules: already current\n  service policy: installed\n  service policy permissions: set to 0440\n  udev rules: reloaded and triggered"
        );
    }

    #[test]
    fn stages_the_service_policy_at_mode_0440_with_its_content() {
        let dir = temporary_directory();
        fs::create_dir_all(&dir).expect("staging directory");
        let contents = "pi ALL=(root) NOPASSWD: /bin/systemctl is-active klipper\n";

        let staged = stage_sudoers(&dir, contents).expect("staging should succeed");

        assert_eq!(
            fs::read_to_string(staged.path()).expect("staged file should be readable"),
            contents
        );
        let mode = fs::metadata(staged.path())
            .expect("staged file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o440);
        assert_eq!(staged.path().parent(), Some(dir.as_path()));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aldis-setup-{}-{nonce}", std::process::id()))
    }
}
