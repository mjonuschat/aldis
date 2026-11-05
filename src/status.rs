//! Klipper MCU status reporting.

use std::process::ExitCode;

use aldis::checkout::RefreshResult;
use aldis::eligibility::{
    CheckoutRevision, Eligibility, RevisionStatus, TargetVersions, assess_mcu,
};
use aldis::moonraker::{McuInventory, McuTransport, MoonrakerAdapter, MoonrakerPort};
use aldis::prebuilt::Running;
use anyhow::Context;

use crate::cli::ConnectionArgs;
use crate::fail;

pub(crate) fn status(arguments: ConnectionArgs) -> ExitCode {
    let source = arguments
        .klipper_source
        .unwrap_or_else(crate::default_klipper_source);
    let moonraker = MoonrakerAdapter::new(&arguments.moonraker.moonraker);
    let revision = aldis::checkout::revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    tracing::debug!(?revision, "checkout revision resolved");
    let versions = TargetVersions::klipper(revision);
    match status_report(
        &arguments.moonraker.moonraker,
        &moonraker,
        &aldis::foci::GithubReleases::new(),
        versions,
        &source,
        arguments.beacon_source.as_deref(),
    ) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("{error:#}")),
    }
}

fn status_report(
    url: &str,
    moonraker: &(impl MoonrakerPort + aldis::moonraker::HostPort),
    releases: &impl aldis::foci::ReleasePort,
    versions: TargetVersions,
    source: &std::path::Path,
    beacon_source: Option<&std::path::Path>,
) -> anyhow::Result<String> {
    tracing::info!("discovering MCUs from Moonraker");
    let mut inventory =
        crate::discovery::discover_mcus_with_retry(moonraker).context("failed to discover MCUs")?;
    crate::discovery::probe_unreported(url, moonraker, &mut inventory);
    let versions = versions.with_prebuilt(crate::discovery::prebuilt_targets(
        &inventory,
        releases,
        beacon_source,
        source,
    ));
    Ok(format_status(source, &versions, &inventory, None))
}

pub(crate) fn format_status(
    source: &std::path::Path,
    versions: &TargetVersions,
    inventory: &McuInventory,
    refreshed: Option<&RefreshResult>,
) -> String {
    let revision = refreshed.map_or_else(|| versions.klipper.label().to_owned(), refresh_label);
    let mut output = format!(
        "Klipper source:    {}\nCheckout revision: {}\n",
        source.display(),
        revision,
    );
    output.push_str(&versions.prebuilt.status_header(inventory));
    for mcu in &inventory.mcus {
        let status = assess_mcu(mcu, versions);
        output.push_str(&format!("\n{}\n", mcu.name));
        for (label, value) in [
            ("firmware:", firmware_label(mcu, &status.eligibility)),
            (
                "version:",
                mcu.version.as_deref().unwrap_or("unknown").to_owned(),
            ),
            ("model:", mcu.mcu.clone()),
            ("connection:", connection_label(mcu.transport.as_ref())),
            (
                "supported:",
                supported_label(mcu, &status.eligibility, versions),
            ),
            ("needs update:", update_label(status.revision)),
        ] {
            output.push_str(&format!("  {label:<13} {value}\n"));
        }
    }
    for unreported in &inventory.unreported {
        output.push_str(&format!("\n{}\n", unreported.name));
        for (label, value) in [
            (
                "connection:",
                connection_label(unreported.transport.as_ref()),
            ),
            ("supported:", format!("no ({})", unreported.reason)),
        ] {
            output.push_str(&format!("  {label:<13} {value}\n"));
        }
    }
    output
}

fn firmware_label(mcu: &aldis::moonraker::Mcu, eligibility: &Eligibility) -> String {
    match eligibility {
        Eligibility::ExternallyManaged(name) => return name.clone(),
        Eligibility::Prebuilt(Running::Beacon(_)) => return "Beacon".to_owned(),
        _ => {}
    }
    if let Some(app) = mcu.app.as_deref().filter(|app| !app.trim().is_empty()) {
        app.to_owned()
    } else if !mcu.kconfig.trim().is_empty() {
        "Klipper".to_owned()
    } else {
        "unknown".to_owned()
    }
}

fn connection_label(transport: Option<&McuTransport>) -> String {
    transport.map_or_else(|| "not configured".to_owned(), McuTransport::describe)
}

fn supported_label(
    mcu: &aldis::moonraker::Mcu,
    eligibility: &Eligibility,
    versions: &TargetVersions,
) -> String {
    match eligibility {
        Eligibility::Prebuilt(running) => running.supported_label(&versions.prebuilt, &mcu.name),
        Eligibility::Eligible => "yes".to_owned(),
        Eligibility::ExternallyManaged(app) => format!("no (third-party firmware: {app})"),
        Eligibility::Unsupported(reason) => format!("no ({reason})"),
        Eligibility::UnsupportedMcu(family) => {
            format!("no ({family} boards have no bootloader aldis can flash)")
        }
    }
}

fn update_label(revision: Option<RevisionStatus>) -> String {
    match revision {
        Some(RevisionStatus::Current) => "no".to_owned(),
        Some(RevisionStatus::UpdateRequired) => "yes".to_owned(),
        Some(RevisionStatus::Indeterminate) => "unknown".to_owned(),
        None => "n/a".to_owned(),
    }
}

pub(crate) fn refresh_label(refreshed: &RefreshResult) -> String {
    let suffix = if refreshed.commits_advanced == 1 {
        "commit"
    } else {
        "commits"
    };
    format!(
        "{} -> {} ({} {suffix})",
        refreshed.before.label(),
        refreshed.after.label(),
        refreshed.commits_advanced,
    )
}

#[cfg(test)]
mod tests {
    use super::{format_status, status_report};
    use aldis::checkout::RefreshResult;
    use aldis::eligibility::{CheckoutRevision, TargetVersions};
    use aldis::foci::{Release, ReleaseError, ReleasePort};
    use aldis::moonraker::{Mcu, McuInventory, McuTransport, MoonrakerError, MoonrakerPort};

    #[test]
    fn renders_human_readable_klipper_status() {
        let inventory = McuInventory {
            mcus: vec![mcu(
                "mcu",
                "v0.12.0-123-deadbeef",
                Some("/dev/serial/by-id/mcu"),
            )],
            unreported: Vec::new(),
        };

        assert_eq!(
            format_status(
                std::path::Path::new("/home/pi/klipper"),
                &TargetVersions::klipper(CheckoutRevision::Known(
                    "v0.13.0-756-g2d7717e3".to_owned()
                )),
                &inventory,
                None,
            ),
            concat!(
                "Klipper source:    /home/pi/klipper\n",
                "Checkout revision: v0.13.0-756-g2d7717e3\n\n",
                "mcu\n",
                "  firmware:     Klipper\n",
                "  version:      v0.12.0-123-deadbeef\n",
                "  model:        test\n",
                "  connection:   serial (/dev/serial/by-id/mcu)\n",
                "  supported:    yes\n",
                "  needs update: yes\n",
            )
        );
    }

    #[test]
    fn folds_the_unsupported_reason_into_the_supported_line() {
        let inventory = McuInventory {
            mcus: vec![
                Mcu {
                    name: "cartographer".to_owned(),
                    app: Some("Cartographer".to_owned()),
                    version: Some("CARTOGRAPHER V4 6.1.0".to_owned()),
                    mcu: "stm32g431xx".to_owned(),
                    canbus_frequency_hz: None,
                    build_versions: None,
                    transport: None,
                    kconfig: String::new(),
                },
                Mcu {
                    name: "old-mcu".to_owned(),
                    app: Some("Klipper".to_owned()),
                    version: Some("v0.12.0-500-gdeadbeef".to_owned()),
                    mcu: "rp2040".to_owned(),
                    canbus_frequency_hz: None,
                    build_versions: None,
                    transport: None,
                    kconfig: String::new(),
                },
            ],
            unreported: Vec::new(),
        };

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-753-g8c29c0a8e".to_owned())),
            &inventory,
            None,
        );

        assert!(output.contains("  firmware:     Cartographer\n"));
        assert!(output.contains("  supported:    no (third-party firmware: Cartographer)\n"));
        assert!(output.contains(
            "  supported:    no (Klipper version too old (v0.12.0-500-gdeadbeef < v0.13.0-753-g8c29c0a8e))\n"
        ));
    }

    #[test]
    fn shows_the_beacon_checkout_and_the_image_each_beacon_takes() {
        let beacon = |name: &str| Mcu {
            name: name.to_owned(),
            app: Some(String::new()),
            version: Some("Beacon 2.0.1".to_owned()),
            mcu: "samd21g18a".to_owned(),
            canbus_frequency_hz: None,
            build_versions: None,
            transport: Some(McuTransport::Serial {
                device: "/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00".to_owned(),
            }),
            kconfig: String::new(),
        };
        let inventory = McuInventory {
            mcus: vec![beacon("mcu beacon"), beacon("mcu beacon sensor rear")],
            unreported: Vec::new(),
        };
        let checkout = std::path::PathBuf::from("/home/pi/beacon_klipper");
        let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
            .with_beacon_targets(aldis::beacon::Targets {
                checkout: Some(checkout.clone()),
                mcus: [
                    (
                        "mcu beacon".to_owned(),
                        Ok(aldis::beacon::Target {
                            revision: "h".to_owned(),
                            path: checkout.join("firmware/revh.dfu"),
                            version: aldis::beacon::version_from_bcd(0x2010),
                        }),
                    ),
                    (
                        "mcu beacon sensor rear".to_owned(),
                        Err(aldis::beacon::Unavailable::UnknownRevision(
                            "Beacon Classic".to_owned(),
                        )),
                    ),
                ]
                .into(),
            });

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &versions,
            &inventory,
            None,
        );

        assert!(
            output.contains("Beacon firmware:   /home/pi/beacon_klipper\n"),
            "{output}"
        );
        assert!(
            output.contains(concat!(
                "mcu beacon\n",
                "  firmware:     Beacon\n",
                "  version:      Beacon 2.0.1\n",
                "  model:        samd21g18a\n",
                "  connection:   serial (/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00)\n",
                "  supported:    yes (revh.dfu holds Beacon 2.1.0)\n",
                "  needs update: yes\n",
            )),
            "{output}"
        );
        assert!(
            output.contains(concat!(
                "  supported:    yes (no Beacon image known: its USB product name ",
                "\"Beacon Classic\" names no Beacon revision)\n",
                "  needs update: unknown\n",
            )),
            "{output}"
        );

        let missing = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
            &inventory,
            None,
        );
        assert!(
            missing.contains("Beacon firmware:   no beacon_klipper checkout found\n"),
            "{missing}"
        );
    }

    /// A mixed inventory of every kind of MCU aldis tells apart, so a change to the shared
    /// prebuilt-firmware path cannot alter any line of `aldis status` unnoticed.
    #[test]
    fn prints_a_mixed_inventory_of_klipper_and_prebuilt_firmware() {
        let usb = |device: &str| {
            Some(McuTransport::Serial {
                device: device.to_owned(),
            })
        };
        let base = |name: &str, app: &str, version: &str, model: &str| Mcu {
            name: name.to_owned(),
            app: Some(app.to_owned()),
            version: Some(version.to_owned()),
            mcu: model.to_owned(),
            canbus_frequency_hz: None,
            build_versions: None,
            transport: None,
            kconfig: String::new(),
        };
        let mut foci = base("mcu ouroboros", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32h723xx");
        foci.build_versions = Some("rust=1.98.1 features=none board=0fb68e1b".to_owned());
        foci.transport = usb("/dev/serial/by-id/usb-foci_Ouroboros-if00");
        let mut foci_dev = base("mcu servo", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32f407xx");
        foci_dev.build_versions = Some("rust=1.98.1 features=dev board=0fb68e1b".to_owned());
        let mut beacon = base("mcu beacon", "", "Beacon 2.0.1", "samd21g18a");
        beacon.transport = usb("/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00");
        let beacon_beta = base(
            "mcu beacon sensor beta",
            "",
            "Beacon 2.2.0-beta",
            "samd21g18a",
        );
        let beacon_none = base("mcu beacon sensor rear", "", "Beacon 2.0.1", "samd21g18a");
        let inventory = McuInventory {
            mcus: vec![
                mcu(
                    "mcu",
                    "v1",
                    Some("/dev/serial/by-id/usb-Klipper_stm32-if00"),
                ),
                foci,
                foci_dev,
                beacon,
                beacon_beta,
                beacon_none,
            ],
            unreported: Vec::new(),
        };
        let beacon_checkout = std::path::PathBuf::from("/home/pi/beacon_klipper");
        let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
            .with_foci_release(Release::from_tag("v0.4.1"))
            .with_beacon_targets(aldis::beacon::Targets {
                checkout: Some(beacon_checkout.clone()),
                mcus: [
                    (
                        "mcu beacon".to_owned(),
                        Ok(aldis::beacon::Target {
                            revision: "h".to_owned(),
                            path: beacon_checkout.join("firmware/revh.dfu"),
                            version: aldis::beacon::version_from_bcd(0x2010),
                        }),
                    ),
                    (
                        "mcu beacon sensor rear".to_owned(),
                        Err(aldis::beacon::Unavailable::NoProduct("/dev/x".to_owned())),
                    ),
                ]
                .into(),
            });

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &versions,
            &inventory,
            None,
        );

        assert_eq!(
            output,
            concat!(
                "Klipper source:    /home/pi/klipper\n",
                "Checkout revision: v2\n",
                "FOCI release:      v0.4.1\n",
                "Beacon firmware:   /home/pi/beacon_klipper\n",
                "\n",
                "mcu\n",
                "  firmware:     Klipper\n",
                "  version:      v1\n",
                "  model:        test\n",
                "  connection:   serial (/dev/serial/by-id/usb-Klipper_stm32-if00)\n",
                "  supported:    yes\n",
                "  needs update: yes\n",
                "\n",
                "mcu ouroboros\n",
                "  firmware:     FOCI\n",
                "  version:      v0.4.0-0-g0fb68e1b\n",
                "  model:        stm32h723xx\n",
                "  connection:   serial (/dev/serial/by-id/usb-foci_Ouroboros-if00)\n",
                "  supported:    yes (FOCI ouroboros-fw prod release builds)\n",
                "  needs update: yes\n",
                "\n",
                "mcu servo\n",
                "  firmware:     FOCI\n",
                "  version:      v0.4.0-0-g0fb68e1b\n",
                "  model:        stm32f407xx\n",
                "  connection:   not configured\n",
                "  supported:    no (FOCI build features \"dev\" match no release flavor)\n",
                "  needs update: n/a\n",
                "\n",
                "mcu beacon\n",
                "  firmware:     Beacon\n",
                "  version:      Beacon 2.0.1\n",
                "  model:        samd21g18a\n",
                "  connection:   serial (/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00)\n",
                "  supported:    yes (revh.dfu holds Beacon 2.1.0)\n",
                "  needs update: yes\n",
                "\n",
                "mcu beacon sensor beta\n",
                "  firmware:     Beacon\n",
                "  version:      Beacon 2.2.0-beta\n",
                "  model:        samd21g18a\n",
                "  connection:   not configured\n",
                "  supported:    no (third-party firmware: Beacon)\n",
                "  needs update: n/a\n",
                "\n",
                "mcu beacon sensor rear\n",
                "  firmware:     Beacon\n",
                "  version:      Beacon 2.0.1\n",
                "  model:        samd21g18a\n",
                "  connection:   not configured\n",
                "  supported:    yes (no Beacon image known: could not read the USB product name of /dev/x)\n",
                "  needs update: unknown\n",
            )
        );
    }

    #[test]
    fn lists_unreported_mcus_with_their_reason() {
        use aldis::moonraker::{UnreportedMcu, UnreportedReason};

        let inventory = McuInventory {
            mcus: Vec::new(),
            unreported: vec![UnreportedMcu {
                name: "mcu xiao".to_owned(),
                transport: None,
                reason: UnreportedReason::NotResponding,
            }],
        };

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned())),
            &inventory,
            None,
        );

        assert!(output.contains("\nmcu xiao\n"), "{output}");
        assert!(
            output.contains("not responding; run `aldis reboot` or power-cycle the board"),
            "{output}"
        );
    }

    #[test]
    fn includes_refresh_details_in_the_aligned_checkout_field() {
        let refreshed = RefreshResult {
            before: CheckoutRevision::Known("v0.12.0-123-deadbeef".to_owned()),
            after: CheckoutRevision::Known("v0.13.0-756-g2d7717e3".to_owned()),
            advanced: true,
            commits_advanced: 4,
        };

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &TargetVersions::klipper(refreshed.after.clone()),
            &McuInventory {
                mcus: Vec::new(),
                unreported: Vec::new(),
            },
            Some(&refreshed),
        );

        assert_eq!(
            output,
            concat!(
                "Klipper source:    /home/pi/klipper\n",
                "Checkout revision: v0.12.0-123-deadbeef -> v0.13.0-756-g2d7717e3 (4 commits)\n",
            )
        );
    }

    #[test]
    fn reports_status_from_an_injected_moonraker_and_a_known_revision() {
        let moonraker = FakeMoonraker(Ok(McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
            unreported: Vec::new(),
        }));
        let report = status_report(
            "http://127.0.0.1:7125",
            &moonraker,
            &NoReleases,
            TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
            std::path::Path::new("/klipper"),
            None,
        )
        .expect("status report should succeed");

        assert!(report.contains("mcu h723"));
        assert!(report.contains("v2"));
    }

    #[test]
    fn reports_the_moonraker_error_when_discovery_fails() {
        let moonraker = FakeMoonraker(Err("no MCU objects were reported".to_owned()));
        let error = status_report(
            "http://127.0.0.1:7125",
            &moonraker,
            &NoReleases,
            TargetVersions::indeterminate(),
            std::path::Path::new("/klipper"),
            None,
        )
        .expect_err("status report should fail");

        assert!(format!("{error:#}").contains("no MCU objects were reported"));
    }

    #[test]
    fn compares_foci_mcus_with_the_latest_foci_release() {
        let mut foci = mcu("mcu ouroboros", "v0.4.0-0-g0fb68e1b", None);
        foci.app = Some("FOCI".to_owned());
        foci.mcu = "stm32h723xx".to_owned();
        foci.kconfig = String::new();
        foci.build_versions = Some("rust=1.98.1 features=none board=0fb68e1b".to_owned());
        let moonraker = FakeMoonraker(Ok(McuInventory {
            mcus: vec![mcu("mcu", "v2", None), foci],
            unreported: Vec::new(),
        }));
        let report = |releases: &dyn Fn() -> Result<Release, ReleaseError>| {
            status_report(
                "http://127.0.0.1:7125",
                &moonraker,
                &FakeReleases(releases),
                TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
                std::path::Path::new("/klipper"),
                None,
            )
            .expect("status report should succeed")
        };

        let found = report(&|| Ok(Release::from_tag("v0.4.1").unwrap()));
        assert!(found.contains("FOCI release:      v0.4.1\n"), "{found}");
        assert!(
            found.contains(
                "mcu ouroboros\n  firmware:     FOCI\n  version:      v0.4.0-0-g0fb68e1b\n"
            ),
            "{found}"
        );
        assert!(
            found.contains(
                "  supported:    yes (FOCI ouroboros-fw prod release builds)\n  needs update: yes\n"
            ),
            "{found}"
        );

        let unknown = report(&|| Err(ReleaseError::InvalidTag("nightly".to_owned())));
        assert!(
            unknown.contains("FOCI release:      unknown\n"),
            "{unknown}"
        );
        assert!(unknown.contains("  needs update: unknown\n"), "{unknown}");
    }

    #[test]
    fn neither_looks_up_nor_shows_a_foci_release_without_a_foci_mcu() {
        let moonraker = FakeMoonraker(Ok(McuInventory {
            mcus: vec![mcu("mcu", "v2", None)],
            unreported: Vec::new(),
        }));
        let report = status_report(
            "http://127.0.0.1:7125",
            &moonraker,
            &NoReleases,
            TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
            std::path::Path::new("/klipper"),
            None,
        )
        .expect("status report should succeed");

        assert!(!report.contains("FOCI"), "{report}");
    }

    /// A release source that must not be asked.
    struct NoReleases;

    impl ReleasePort for NoReleases {
        fn latest_release(&self) -> Result<Release, ReleaseError> {
            panic!("no FOCI MCU, so no release lookup");
        }

        fn download_firmware(&self, _: &Release, _: &str) -> Result<Vec<u8>, ReleaseError> {
            panic!("status never downloads firmware");
        }
    }

    struct FakeReleases<'a>(&'a dyn Fn() -> Result<Release, ReleaseError>);

    impl ReleasePort for FakeReleases<'_> {
        fn latest_release(&self) -> Result<Release, ReleaseError> {
            (self.0)()
        }

        fn download_firmware(&self, _: &Release, _: &str) -> Result<Vec<u8>, ReleaseError> {
            panic!("status never downloads firmware");
        }
    }

    struct FakeMoonraker(Result<McuInventory, String>);

    impl MoonrakerPort for FakeMoonraker {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            self.0.clone().map_err(MoonrakerError::InvalidResponse)
        }
    }

    impl aldis::moonraker::HostPort for FakeMoonraker {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Ok(None)
        }

        fn host_info(&self) -> Result<aldis::moonraker::HostInfo, MoonrakerError> {
            Ok(aldis::moonraker::HostInfo {
                state: aldis::moonraker::KlippyState::Ready,
                state_message: String::new(),
                software_version: None,
                klipper_path: None,
                python_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    fn mcu(name: &str, version: &str, serial: Option<&str>) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: None,
            version: Some(version.to_owned()),
            mcu: "test".to_owned(),
            canbus_frequency_hz: None,
            build_versions: None,
            transport: serial.map(|device| McuTransport::Serial {
                device: device.to_owned(),
            }),
            kconfig: "CONFIG_TEST=y\n".to_owned(),
        }
    }
}
