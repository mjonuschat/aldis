//! Host permission setup: udev rules and the sudoers policy for the Klipper service.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use aldis::flash::linux_host::{INSTALL_COMMAND, RESTART_COMMAND};
use anyhow::{Context, bail};

use crate::cli::SetupArgs;
use crate::fail;

const UDEV_RULES_PATH: &str = "/etc/udev/rules.d/80-aldis.rules";
const SUDOERS_PATH: &str = "/etc/sudoers.d/aldis";
const UDEV_RULES: &str = include_str!("../templates/80-aldis.rules");
pub(crate) const AGENT_UNIT_PATH: &str = "/etc/systemd/system/aldis.service";
const AGENT_SERVICE: &str = "aldis";

pub(crate) fn setup(arguments: SetupArgs) -> ExitCode {
    if arguments.check {
        return check_setup();
    }
    let result = if arguments.remove {
        remove_agent(&arguments.moonraker.moonraker)
    } else {
        install_setup().and_then(|user| {
            if arguments.agent {
                install_agent(&user, &arguments.moonraker.moonraker)
            } else {
                Ok(())
            }
        })
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(format!("{error:#}")),
    }
}

fn install_setup() -> anyhow::Result<String> {
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
    Ok(user)
}

fn agent_unit(user: &str, exe: &Path, moonraker: &str) -> String {
    format!(
        "[Unit]\nDescription=aldis Moonraker agent\nAfter=moonraker.service\nWants=moonraker.service\n\n\
         [Service]\nType=simple\nUser={user}\nExecStart={} agent --moonraker {moonraker}\n\
         Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n",
        exe.display()
    )
}

/// `asvc` with `name` appended, or `None` if it is already listed.
fn with_service(asvc: &str, name: &str) -> Option<String> {
    if asvc.lines().any(|line| line.trim() == name) {
        return None;
    }
    let mut updated = asvc.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(name);
    updated.push('\n');
    Some(updated)
}

/// `asvc` without `name`, or `None` if it is not listed.
fn without_service(asvc: &str, name: &str) -> Option<String> {
    if !asvc.lines().any(|line| line.trim() == name) {
        return None;
    }
    Some(
        asvc.lines()
            .filter(|line| line.trim() != name)
            .map(|line| format!("{line}\n"))
            .collect(),
    )
}

fn moonraker_asvc(moonraker: &str) -> Option<std::path::PathBuf> {
    use aldis::moonraker::HostPort;
    let logs = aldis::moonraker::MoonrakerAdapter::new(moonraker)
        .logs_root()
        .ok()
        .flatten()?;
    Some(logs.parent()?.join("moonraker.asvc"))
}

fn systemctl(arguments: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("systemctl")
        .args(arguments)
        .status()
        .with_context(|| format!("could not run systemctl {}", arguments.join(" ")))?;
    if !status.success() {
        bail!("systemctl {} exited with {status}", arguments.join(" "));
    }
    Ok(())
}

fn install_agent(user: &str, moonraker: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("could not resolve the aldis binary path")?;
    let unit_action = install_file(
        AGENT_UNIT_PATH,
        &agent_unit(user, &exe, moonraker),
        "agent service",
    )?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", AGENT_SERVICE])?;
    // restart rather than start: a repaired unit (new binary path or URL) must take effect.
    systemctl(&["restart", AGENT_SERVICE])?;
    let asvc_action = match moonraker_asvc(moonraker) {
        Some(path) if path.exists() => {
            let current = fs::read_to_string(&path).context("could not read moonraker.asvc")?;
            match with_service(&current, AGENT_SERVICE) {
                Some(updated) => {
                    fs::write(&path, updated).context("could not update moonraker.asvc")?;
                    format!(
                        "added aldis to {}; restart Moonraker for it to appear in the service menu",
                        path.display()
                    )
                }
                None => format!("already listed in {}", path.display()),
            }
        }
        Some(path) => format!("skipped: {} does not exist", path.display()),
        None => "skipped: could not ask Moonraker for its data directory".to_owned(),
    };
    println!(
        "aldis agent:\n  service: {unit_action}, enabled and restarted\n  Moonraker service list: {asvc_action}"
    );
    Ok(())
}

fn remove_agent(moonraker: &str) -> anyhow::Result<()> {
    if Path::new(AGENT_UNIT_PATH).exists() {
        systemctl(&["disable", "--now", AGENT_SERVICE])?;
        fs::remove_file(AGENT_UNIT_PATH).context("could not remove the agent service")?;
        systemctl(&["daemon-reload"])?;
    }
    if let Some(path) = moonraker_asvc(moonraker).filter(|path| path.exists()) {
        let current = fs::read_to_string(&path).context("could not read moonraker.asvc")?;
        if let Some(updated) = without_service(&current, AGENT_SERVICE) {
            fs::write(&path, updated).context("could not update moonraker.asvc")?;
            println!(
                "removed aldis from {}; restart Moonraker to drop it from the service menu",
                path.display()
            );
        }
    }
    println!("aldis agent: removed");
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
    let host_mcu = Command::new("sudo")
        .args(["-n", "-l"])
        .args(INSTALL_COMMAND)
        .output()
        .is_ok_and(|output| output.status.success());
    tracing::debug!(rules, service, host_mcu, "setup check results");
    println!("{}", setup_check_report(rules, service, host_mcu));
    if rules && service {
        ExitCode::SUCCESS
    } else {
        fail("aldis setup is incomplete; run sudo aldis setup".to_owned())
    }
}

fn setup_check_report(rules: bool, service: bool, host_mcu: bool) -> String {
    format!(
        "aldis setup:\n  udev rules: {}\n  Klipper service access: {}\n  host MCU install: {}",
        if rules {
            "ready"
        } else {
            "missing or outdated"
        },
        if service { "ready" } else { "unavailable" },
        if host_mcu { "ready" } else { "unavailable" },
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
        "{user} ALL=(root) NOPASSWD: /bin/systemctl is-active klipper, /bin/systemctl stop klipper, \
         /bin/systemctl start klipper, {}, {}\n",
        INSTALL_COMMAND.join(" "),
        RESTART_COMMAND.join(" "),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        agent_unit, setup_check_report, setup_install_report, stage_sudoers,
        sudo_policy_allows_service_status, sudoers_policy, with_service, without_service,
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
            setup_check_report(true, false, false),
            "aldis setup:\n  udev rules: ready\n  Klipper service access: unavailable\n  host MCU install: unavailable"
        );
        assert!(setup_check_report(true, true, true).ends_with("host MCU install: ready"));
    }

    #[test]
    fn grants_exactly_the_fixed_host_mcu_commands() {
        assert_eq!(
            sudoers_policy("pi"),
            "pi ALL=(root) NOPASSWD: /bin/systemctl is-active klipper, /bin/systemctl stop klipper, \
             /bin/systemctl start klipper, /usr/bin/install -m 0755 /dev/stdin /usr/local/bin/klipper_mcu, \
             /bin/systemctl restart klipper-mcu\n"
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

    #[test]
    fn renders_the_agent_unit_for_the_sudo_user() {
        let unit = agent_unit(
            "pi",
            std::path::Path::new("/home/pi/aldis/aldis"),
            "http://127.0.0.1:7125",
        );

        assert!(unit.contains("\nUser=pi\n"), "{unit}");
        assert!(
            unit.contains(
                "\nExecStart=/home/pi/aldis/aldis agent --moonraker http://127.0.0.1:7125\n"
            ),
            "{unit}"
        );
        assert!(unit.contains("\nRestart=always\n"), "{unit}");
        assert!(unit.contains("\nAfter=moonraker.service\n"), "{unit}");
    }

    #[test]
    fn adds_and_removes_the_agent_in_moonrakers_service_list() {
        let asvc = "klipper_mcu\nwebcamd\nMoonCord\n";

        let added = with_service(asvc, "aldis").unwrap();
        assert_eq!(added, "klipper_mcu\nwebcamd\nMoonCord\naldis\n");
        assert_eq!(with_service(&added, "aldis"), None);
        assert_eq!(
            with_service("klipper_mcu", "aldis").unwrap(),
            "klipper_mcu\naldis\n"
        );

        assert_eq!(without_service(&added, "aldis").unwrap(), asvc);
        assert_eq!(without_service(asvc, "aldis"), None);
    }

    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aldis-setup-{}-{nonce}", std::process::id()))
    }
}
