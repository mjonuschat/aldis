//! Klipper MCU status reporting.

use std::process::ExitCode;

use aldis::checkout::{CheckoutPort, GitCheckoutAdapter, RefreshResult};
use aldis::eligibility::{CheckoutRevision, Eligibility, RevisionStatus, assess_mcu};
use aldis::moonraker::{McuInventory, McuTransport, MoonrakerAdapter, MoonrakerPort};
use anyhow::Context;

use crate::cli::ConnectionArgs;
use crate::fail;

pub(crate) fn status(arguments: ConnectionArgs) -> ExitCode {
    let source = arguments
        .klipper_source
        .unwrap_or_else(crate::default_klipper_source);
    let moonraker = MoonrakerAdapter::new(&arguments.moonraker.moonraker);
    match status_report(&moonraker, &GitCheckoutAdapter, &source) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("{error:#}")),
    }
}

fn status_report(
    moonraker: &impl MoonrakerPort,
    checkout: &impl CheckoutPort,
    source: &std::path::Path,
) -> anyhow::Result<String> {
    tracing::info!("discovering MCUs from Moonraker");
    let inventory =
        crate::discovery::discover_mcus_with_retry(moonraker).context("failed to discover MCUs")?;
    let revision = checkout
        .revision(source)
        .unwrap_or(CheckoutRevision::Indeterminate);
    tracing::debug!(?revision, "checkout revision resolved");
    Ok(format_status(source, &revision, &inventory, None))
}

pub(crate) fn format_status(
    source: &std::path::Path,
    checkout: &CheckoutRevision,
    inventory: &McuInventory,
    refreshed: Option<&RefreshResult>,
) -> String {
    let revision = refreshed.map_or_else(|| checkout_label(checkout).to_owned(), refresh_label);
    let mut output = format!(
        "Klipper source:    {}\nCheckout revision: {}\n",
        source.display(),
        revision,
    );
    for mcu in &inventory.mcus {
        let status = assess_mcu(mcu, checkout);
        output.push_str(&format!("\n{}\n", mcu.name));
        for (label, value) in [
            ("firmware:", firmware_label(mcu)),
            (
                "version:",
                mcu.version.as_deref().unwrap_or("unknown").to_owned(),
            ),
            ("model:", mcu.mcu.clone()),
            ("connection:", connection_label(mcu.transport.as_ref())),
            ("supported:", supported_label(status.eligibility)),
            ("needs update:", update_label(status.revision)),
        ] {
            output.push_str(&format!("  {label:<13} {value}\n"));
        }
    }
    output
}

pub(crate) fn checkout_label(checkout: &CheckoutRevision) -> &str {
    match checkout {
        CheckoutRevision::Known(revision) => revision,
        CheckoutRevision::Indeterminate => "unknown",
    }
}

fn firmware_label(mcu: &aldis::moonraker::Mcu) -> String {
    if let Some(app) = mcu.app.as_deref().filter(|app| !app.trim().is_empty()) {
        app.to_owned()
    } else if !mcu.kconfig.trim().is_empty() {
        "Klipper".to_owned()
    } else {
        "unknown".to_owned()
    }
}

fn connection_label(transport: Option<&McuTransport>) -> String {
    match transport {
        Some(McuTransport::Serial { device }) => format!("serial ({device})"),
        Some(McuTransport::Can { interface, uuid }) => format!("CAN ({interface}, {uuid:012x})"),
        None => "not configured".to_owned(),
    }
}

fn supported_label(eligibility: Eligibility) -> String {
    match eligibility {
        Eligibility::Eligible => "yes".to_owned(),
        Eligibility::ExternallyManaged | Eligibility::Unsupported => "no".to_owned(),
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
        checkout_label(&refreshed.before),
        checkout_label(&refreshed.after),
        refreshed.commits_advanced,
    )
}

#[cfg(test)]
mod tests {
    use super::{format_status, status_report};
    use aldis::checkout::{CheckoutError, CheckoutPort, RefreshResult};
    use aldis::eligibility::CheckoutRevision;
    use aldis::moonraker::{Mcu, McuInventory, McuTransport, MoonrakerError, MoonrakerPort};

    #[test]
    fn renders_human_readable_klipper_status() {
        let inventory = McuInventory {
            mcus: vec![mcu(
                "mcu",
                "v0.12.0-123-deadbeef",
                Some("/dev/serial/by-id/mcu"),
            )],
        };

        assert_eq!(
            format_status(
                std::path::Path::new("/home/pi/klipper"),
                &CheckoutRevision::Known("v0.13.0-756-g2d7717e3".to_owned()),
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
    fn includes_refresh_details_in_the_aligned_checkout_field() {
        let refreshed = RefreshResult {
            before: CheckoutRevision::Known("v0.12.0-123-deadbeef".to_owned()),
            after: CheckoutRevision::Known("v0.13.0-756-g2d7717e3".to_owned()),
            advanced: true,
            commits_advanced: 4,
        };

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &refreshed.after,
            &McuInventory { mcus: Vec::new() },
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
    fn reports_status_from_injected_moonraker_and_checkout_sources() {
        let moonraker = FakeMoonraker(Ok(McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
        }));
        let checkout = FakeCheckout(Ok(CheckoutRevision::Known("v2".to_owned())));

        let report = status_report(&moonraker, &checkout, std::path::Path::new("/klipper"))
            .expect("status report should succeed");

        assert!(report.contains("mcu h723"));
        assert!(report.contains("v2"));
    }

    #[test]
    fn reports_the_moonraker_error_when_discovery_fails() {
        let moonraker = FakeMoonraker(Err("no MCU objects were reported".to_owned()));
        let checkout = FakeCheckout(Ok(CheckoutRevision::Indeterminate));

        let error = status_report(&moonraker, &checkout, std::path::Path::new("/klipper"))
            .expect_err("status report should fail");

        assert!(format!("{error:#}").contains("no MCU objects were reported"));
    }

    struct FakeMoonraker(Result<McuInventory, String>);

    impl MoonrakerPort for FakeMoonraker {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            self.0.clone().map_err(MoonrakerError::InvalidResponse)
        }
    }

    struct FakeCheckout(Result<CheckoutRevision, String>);

    impl CheckoutPort for FakeCheckout {
        fn revision(&self, _path: &std::path::Path) -> Result<CheckoutRevision, CheckoutError> {
            self.0
                .clone()
                .map_err(|_| CheckoutError::UpstreamNotConfigured)
        }

        fn refresh(&self, _path: &std::path::Path) -> Result<RefreshResult, CheckoutError> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn mcu(name: &str, version: &str, serial: Option<&str>) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: None,
            version: Some(version.to_owned()),
            mcu: "test".to_owned(),
            canbus_frequency_hz: None,
            transport: serial.map(|device| McuTransport::Serial {
                device: device.to_owned(),
            }),
            kconfig: "CONFIG_TEST=y\n".to_owned(),
        }
    }
}
