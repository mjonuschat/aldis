//! Preparing a file for prebuilt-firmware MCUs, and the coordinator's refusal of a request made
//! for another MCU.

use std::sync::{Arc, Mutex};

use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort};
use aldis::coordinator::{BuildCoordinator, CoordinatorError, FlashCoordinatorError};
use aldis::flash::request::{Protocol, Vendor};
use aldis::moonraker::{Mcu, McuInventory, McuTransport};
use aldis::prebuilt::{ExplicitError, prepare_explicit};
use aldis::workspace::RunWorkspace;

fn mcu(name: &str, app: &str, version: &str, model: &str) -> Mcu {
    Mcu {
        name: name.to_owned(),
        app: Some(app.to_owned()),
        version: Some(version.to_owned()),
        mcu: model.to_owned(),
        canbus_frequency_hz: None,
        build_versions: None,
        transport: Some(McuTransport::Serial {
            device: format!("/dev/serial/by-id/usb-{name}-if00"),
        }),
        kconfig: String::new(),
    }
}

fn foci(name: &str, model: &str) -> Mcu {
    let mut foci = mcu(name, "FOCI", "v0.4.0-0-g0fb68e1b", model);
    foci.build_versions = Some("rust=1.98.1 features=none board=a82de21d".to_owned());
    foci
}

#[test]
fn sends_foci_to_its_boards_katapult() {
    let request = prepare_explicit(&foci("mcu servo", "stm32h723xx"), b"any image")
        .unwrap()
        .expect("FOCI takes any image");
    assert_eq!(request.vendor(), Vendor::Foci);
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            image,
            application_start: 0x0802_0000,
        } if image == b"any image"
    ));

    assert!(matches!(
        prepare_explicit(&foci("mcu servo", "stm32f407xx"), b"any image")
            .unwrap()
            .unwrap()
            .protocol(),
        Protocol::Katapult {
            application_start: 0x0800_8000,
            ..
        }
    ));

    assert_eq!(
        prepare_explicit(&foci("mcu servo", "stm32g474xx"), b"any image"),
        Err(ExplicitError::UnknownFociBoard("stm32g474xx".to_owned())),
        "a recognized MCU the firmware cannot be placed for never falls through"
    );
}

#[test]
fn lets_any_file_through_for_firmware_it_builds() {
    let klipper = mcu("mcu", "Klipper", "v0.13.0-770-gce7002bed", "stm32h723xx");
    assert_eq!(prepare_explicit(&klipper, b"anything"), Ok(None));
}

#[derive(Clone, Default)]
struct RecordingRunner(Arc<Mutex<Vec<BuildCommand>>>);

impl CommandPort for RecordingRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.0.lock().unwrap().push(command.clone());
        Ok(CommandOutput {
            success: false,
            stdout: b"inactive\n".to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[test]
fn refuses_a_request_prepared_for_another_mcu_before_touching_klipper() {
    let first = foci("mcu first", "stm32h723xx");
    let second = foci("mcu second", "stm32h723xx");
    let request = prepare_explicit(&first, b"any image").unwrap().unwrap();
    let inventory = McuInventory {
        mcus: vec![first, second],
        unreported: Vec::new(),
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
    let runner = RecordingRunner::default();
    let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
    let approved = coordinator
        .prepare(&inventory, &workspace, "mcu second", false)
        .unwrap()
        .approve();

    let error = coordinator
        .flash_prebuilt(
            approved,
            &request,
            aldis::update_run::standard_flash_options(),
            |_| {},
        )
        .unwrap_err();

    assert!(
        matches!(
            &error,
            FlashCoordinatorError::Coordinator(CoordinatorError::RequestForAnotherMcu {
                requested, approved
            }) if requested == "mcu first" && approved == "mcu second"
        ),
        "{error:?}"
    );
    assert!(
        runner.0.lock().unwrap().is_empty(),
        "Klipper is neither queried nor stopped"
    );
}

struct FakeReleases;

impl aldis::foci::ReleasePort for FakeReleases {
    fn latest_release(&self) -> Result<aldis::foci::Release, aldis::foci::ReleaseError> {
        panic!("a run never looks up a release");
    }

    fn download_firmware(
        &self,
        _: &aldis::foci::Release,
        _: &str,
    ) -> Result<Vec<u8>, aldis::foci::ReleaseError> {
        Ok(b"release image".to_vec())
    }
}

#[test]
fn prepares_a_foci_update_for_katapult_and_leaves_klipper_firmware_to_the_build() {
    use aldis::eligibility::{CheckoutRevision, TargetVersions};
    use aldis::prebuilt::prepare_automatic;

    let dir = tempfile::tempdir().unwrap();
    let foci = foci("mcu foci", "stm32h723xx");
    let klipper = mcu("mcu", "Klipper", "v0.13.0-770-gce7002bed", "stm32h723xx");
    let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
        .with_foci_release(aldis::foci::Release::from_tag("v0.4.1"));
    let inventory = McuInventory {
        mcus: vec![foci.clone(), klipper.clone()],
        unreported: Vec::new(),
    };
    let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
    let runner = RecordingRunner::default();
    let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner);
    let prepare = |mcu: &Mcu| {
        let pending = coordinator
            .prepare(&inventory, &workspace, &mcu.name, false)
            .unwrap();
        prepare_automatic(mcu, &versions, &FakeReleases, &pending, &mut |_| {})
    };

    let request = prepare(&foci).unwrap().unwrap();
    assert_eq!(request.vendor(), Vendor::Foci);
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            image,
            application_start: 0x0802_0000,
        } if image == b"release image"
    ));
    assert!(
        prepare(&klipper).is_none(),
        "firmware aldis builds is not prepared here"
    );
}
