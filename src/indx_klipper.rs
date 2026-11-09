//! Building INDX firmware for a Klipper checkout that has none.
//!
//! Kalico ships the INDX heater driver, so its checkout builds INDX firmware as it is. Mainline
//! Klipper does not: Bondtech's `indx_klipper` supplies the driver and a patch for Klipper's build
//! system. Its own `make` applies that to a copy of the Klipper tree, leaving the checkout alone,
//! and aldis does the same with the project's `prepare-klipper-build.py`: the copy lives in the
//! checkout's `out/klipper-indx`, where `make` there builds too, so both keep one set of objects.

use std::path::{Path, PathBuf};

use crate::build::BuildCommand;
use crate::flash::indx_dfu;

/// The patch that gives Klipper's build system the INDX heater driver.
const PATCH: &str = "mcu/patches/klipper-indx.patch";
/// The script that applies the patch to a copy of the Klipper tree.
const PREPARE_SCRIPT: &str = "mcu/scripts/prepare-klipper-build.py";
/// The build configuration the script starts from, in the checkout's `mcu` directory.
const SEED_CONFIG: &str = "bondtech_indx_usb.config";
/// The environment variable naming the checkout, for one that is not found by itself.
pub const SOURCE_ENV: &str = "ALDIS_INDX_SOURCE";

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_owned())
}

/// Whether `path` is an `indx_klipper` checkout that can build firmware.
pub fn is_checkout(path: &Path) -> bool {
    path.join(PATCH).is_file() && path.join(PREPARE_SCRIPT).is_file()
}

/// Whether building `kconfig` in `klipper_source` needs `indx_klipper`: the firmware is an INDX
/// build, and the checkout has no INDX driver of its own, as Kalico's does.
pub fn needs_overlay(kconfig: &str, klipper_source: &Path) -> bool {
    indx_dfu::is_indx_build(kconfig) && !klipper_source.join("src/indx").is_dir()
}

/// Finds the user's `indx_klipper` checkout: `configured` when given, otherwise the directory
/// its `install.sh` linked `klippy/extras/indx` in the Klipper checkout from, otherwise
/// `~/indx_klipper`.
pub fn locate_checkout(
    configured: Option<&Path>,
    klipper_source: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(configured) = configured {
        return is_checkout(configured).then(|| absolute(configured));
    }
    // `install.sh` links `host/`, which sits directly in the checkout.
    let linked = std::fs::canonicalize(klipper_source.join("klippy/extras/indx"))
        .ok()
        .and_then(|host| host.parent().map(Path::to_owned));
    linked
        .into_iter()
        .chain(home.map(|home| home.join("indx_klipper")))
        .find(|path| is_checkout(path))
}

/// The checkout [`locate_checkout`] finds from the environment: `ALDIS_INDX_SOURCE`, the one
/// `klipper_source` links from, or `~/indx_klipper`.
pub fn find_checkout(klipper_source: &Path) -> Option<PathBuf> {
    let configured = std::env::var_os(SOURCE_ENV).map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    locate_checkout(configured.as_deref(), klipper_source, home.as_deref())
}

/// The copy of the Klipper tree that INDX firmware is built in.
pub fn build_dir(checkout: &Path) -> PathBuf {
    checkout.join("out/klipper-indx")
}

/// The command that brings [`build_dir`] up to date with `klipper_source` plus the INDX driver.
pub fn prepare_command(checkout: &Path, klipper_source: &Path) -> BuildCommand {
    BuildCommand {
        program: "python3".to_owned(),
        arguments: vec![
            checkout.join(PREPARE_SCRIPT).display().to_string(),
            absolute(klipper_source).display().to_string(),
            build_dir(checkout).display().to_string(),
            SEED_CONFIG.to_owned(),
        ],
        current_dir: Some(checkout.to_owned()),
        stdin: None,
    }
}

/// Gives the patched copy the Klipper checkout's version. The copy is no git checkout, so Klipper
/// would build firmware that reports `?` as its version, which aldis takes for third-party
/// firmware it must not manage. Klipper reads `klippy/.version` when there is no git history, as
/// distribution packages rely on.
pub fn write_version(build_dir: &Path, revision: &str) -> std::io::Result<()> {
    let path = build_dir.join("klippy/.version");
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == revision) {
        return Ok(());
    }
    std::fs::write(path, revision)
}

/// How many trailing lines of what a failed preparation printed are reported.
const REPORTED_LINES: usize = 12;

/// What a failed preparation printed, as one block: the patch tool writes to standard output and
/// the script's own failure to standard error, and the end of each says what went wrong.
pub fn report(stdout: &[u8], stderr: &[u8]) -> String {
    let lines = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        let all: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        all[all.len().saturating_sub(REPORTED_LINES)..].join("\n")
    };
    [lines(stdout), lines(stderr)]
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDX_KCONFIG: &str = "CONFIG_MACH_SAME51J19=y\nCONFIG_WANT_INDX_HEATER=y\n";

    fn checkout(path: &Path) {
        std::fs::create_dir_all(path.join("mcu/patches")).unwrap();
        std::fs::create_dir_all(path.join("mcu/scripts")).unwrap();
        std::fs::create_dir_all(path.join("host")).unwrap();
        std::fs::write(path.join(PATCH), "").unwrap();
        std::fs::write(path.join(PREPARE_SCRIPT), "").unwrap();
    }

    #[test]
    fn needs_the_overlay_only_for_an_indx_build_in_a_klipper_without_the_driver() {
        let dir = tempfile::tempdir().unwrap();
        assert!(needs_overlay(INDX_KCONFIG, dir.path()));
        assert!(!needs_overlay("CONFIG_MACH_STM32H723=y\n", dir.path()));

        std::fs::create_dir_all(dir.path().join("src/indx")).unwrap();
        assert!(
            !needs_overlay(INDX_KCONFIG, dir.path()),
            "Kalico builds INDX firmware as it is"
        );
    }

    #[test]
    fn finds_the_checkout_klippys_indx_module_is_linked_from() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("elsewhere/indx_klipper");
        checkout(&repo);
        let klipper = dir.path().join("klipper");
        std::fs::create_dir_all(klipper.join("klippy/extras")).unwrap();
        std::os::unix::fs::symlink(repo.join("host"), klipper.join("klippy/extras/indx")).unwrap();

        assert_eq!(
            locate_checkout(None, &klipper, Some(&dir.path().join("home"))),
            Some(std::fs::canonicalize(&repo).unwrap())
        );
    }

    #[test]
    fn falls_back_to_the_home_directory_and_prefers_a_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        checkout(&home.join("indx_klipper"));
        let klipper = dir.path().join("klipper");
        std::fs::create_dir_all(&klipper).unwrap();

        assert_eq!(
            locate_checkout(None, &klipper, Some(&home)),
            Some(home.join("indx_klipper"))
        );

        let configured = dir.path().join("configured");
        checkout(&configured);
        assert_eq!(
            locate_checkout(Some(&configured), &klipper, Some(&home)),
            Some(configured.clone())
        );
        assert_eq!(
            locate_checkout(Some(&dir.path().join("missing")), &klipper, Some(&home)),
            None,
            "a configured path that is no checkout is not replaced by another"
        );
    }

    #[test]
    fn ignores_a_directory_that_is_not_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("indx_klipper/host")).unwrap();
        let klipper = dir.path().join("klipper");
        std::fs::create_dir_all(&klipper).unwrap();

        assert_eq!(locate_checkout(None, &klipper, Some(dir.path())), None);
    }

    #[test]
    fn gives_the_copy_the_checkouts_version_for_klipper_to_report() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("klippy")).unwrap();

        write_version(dir.path(), "v0.13.0-770-gce7002bed").unwrap();
        write_version(dir.path(), "v0.13.0-770-gce7002bed").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("klippy/.version")).unwrap(),
            "v0.13.0-770-gce7002bed"
        );
    }

    #[test]
    fn reports_the_end_of_what_both_streams_printed() {
        let stdout: String = (1..=30).map(|n| format!("out {n}\n")).collect();
        let report = report(stdout.as_bytes(), b"\nboom\n");

        assert!(report.ends_with("out 30\nboom"), "{report}");
        assert!(
            !report.contains("out 18\n"),
            "only the last lines: {report}"
        );
        assert_eq!(super::report(b"", b""), "");
    }

    #[test]
    fn resolves_a_relative_checkout_and_klipper_source_before_the_script_changes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("indx_klipper");
        checkout(&repo);
        let cwd = std::env::current_dir().unwrap();
        let relative = repo.strip_prefix(&cwd).unwrap_or(&repo);

        let found = locate_checkout(Some(relative), Path::new("klipper"), None).unwrap();
        assert!(found.is_absolute(), "{found:?}");

        let command = prepare_command(&found, Path::new("klipper"));
        assert!(
            Path::new(&command.arguments[1]).is_absolute(),
            "{command:?}"
        );
        assert!(
            Path::new(&command.arguments[2]).is_absolute(),
            "{command:?}"
        );
        assert!(
            Path::new(&command.arguments[0]).is_absolute(),
            "{command:?}"
        );
    }

    #[test]
    fn prepares_the_copy_in_the_checkouts_own_build_directory() {
        let command = prepare_command(Path::new("/h/indx_klipper"), Path::new("/h/klipper"));

        assert_eq!(command.program, "python3");
        assert_eq!(
            command.arguments,
            [
                "/h/indx_klipper/mcu/scripts/prepare-klipper-build.py",
                "/h/klipper",
                "/h/indx_klipper/out/klipper-indx",
                "bondtech_indx_usb.config",
            ]
        );
    }
}
