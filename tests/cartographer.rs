use std::io::Write;
use std::path::Path;

use aldis::cartographer::{
    Build, Generation, Mismatch, Target, Unavailable, check_image, compare, identify,
    image_identity, is_cartographer, locate_checkout, parse_manifest, parse_version,
    plugin_version_in, resolve_targets,
};
use aldis::eligibility::RevisionStatus;
use aldis::moonraker::{Mcu, McuInventory, McuTransport};

fn probe(name: &str, app: Option<&str>, version: &str, chip: &str, can: Option<u64>) -> Mcu {
    Mcu {
        name: name.to_owned(),
        app: app.map(str::to_owned),
        version: Some(version.to_owned()),
        mcu: chip.to_owned(),
        canbus_frequency_hz: can,
        build_versions: Some(String::new()),
        transport: Some(match can {
            Some(_) => McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            },
            None => McuTransport::Serial {
                device: "/dev/serial/by-id/usb-Cartographer_614e_ABC-if00".to_owned(),
            },
        }),
        kconfig: String::new(),
    }
}

/// A firmware image: a vector table whose reset handler lies `reset_offset` into flash, then
/// the identify dictionary Klipper's build embeds, zlib-compressed.
fn image(version: &str, chip: &str, can: Option<u64>, reset_offset: u32) -> Vec<u8> {
    image_of("Cartographer", version, chip, can, reset_offset)
}

/// The same, for firmware whose dictionary names itself `app`.
fn image_of(app: &str, version: &str, chip: &str, can: Option<u64>, reset_offset: u32) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(&0x2000_8000_u32.to_le_bytes());
    image.extend_from_slice(&(0x0800_0000 + reset_offset + 1).to_le_bytes());
    image.resize(0x200, 0);
    let mut config = serde_json::json!({ "MCU": chip, "CLOCK_FREQ": 170_000_000 });
    if let Some(can) = can {
        config["CANBUS_FREQUENCY"] = can.into();
    }
    let dictionary = serde_json::json!({
        "version": version,
        "app": app,
        "build_versions": "",
        "config": config,
        "commands": {},
        "responses": {},
    });
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(dictionary.to_string().as_bytes())
        .unwrap();
    image.extend(encoder.finish().unwrap());
    image
}

/// An image linked to start after Cartographer's 8 KiB Katapult.
fn application(version: &str, chip: &str, can: Option<u64>) -> Vec<u8> {
    image(version, chip, can, 0x2100)
}

const HEADER: &str = "filepath,filename,firmware_type,protocol,process,probe_version,\
                      firmware_version,can_speed,is_lite,min_plugin_version";

#[test]
fn parses_the_version_generation_and_build_cartographer_reports() {
    assert_eq!(
        parse_version("CARTOGRAPHER V4 6.1.0"),
        Some(semver::Version::new(6, 1, 0))
    );
    assert_eq!(
        parse_version("Cartographer HT 5.1.0"),
        Some(semver::Version::new(5, 1, 0))
    );
    assert_eq!(parse_version("CARTOGRAPHER V3"), None);
    assert_eq!(parse_version("CARTOGRAPHER 5.1.0 6.0.0"), None);

    assert_eq!(
        Build::from_version("CARTOGRAPHER v4 6.2.0 Lite"),
        Build::Lite
    );
    assert_eq!(
        Build::from_version("CARTOGRAPHER V3 6.1.0 lite"),
        Build::Lite
    );
    assert_eq!(Build::from_version("CARTOGRAPHER K1 5.1.0"), Build::Lite);
    assert_eq!(Build::from_version("CARTOGRAPHER 5.0.0 K1"), Build::Lite);
    assert_eq!(Build::from_version("CARTOGRAPHER V4 6.1.0"), Build::Full);

    assert_eq!(Generation::from_mcu("stm32f042x6"), Some(Generation::V3));
    assert_eq!(Generation::from_mcu("stm32g431xx"), Some(Generation::V4));
    assert_eq!(Generation::from_mcu("stm32g474xx"), Some(Generation::V4));
    assert_eq!(Generation::from_mcu("rp2040"), None);
}

#[test]
fn recognizes_cartographer_by_its_app_or_before_6_1_by_its_version() {
    assert!(is_cartographer(&probe(
        "mcu cartographer",
        Some("Cartographer"),
        "CARTOGRAPHER V4 6.1.0",
        "stm32g431xx",
        None
    )));
    assert!(is_cartographer(&probe(
        "mcu scanner",
        None,
        "Cartographer HT 5.1.0",
        "stm32g431xx",
        None
    )));
    assert!(!is_cartographer(&probe(
        "mcu",
        Some("Klipper"),
        "Cartographer 1.0.0",
        "stm32g431xx",
        None
    )));

    let firmware = identify(&probe(
        "mcu cartographer",
        Some("Cartographer"),
        "CARTOGRAPHER V4 6.1.0 lite",
        "stm32g431xx",
        Some(1_000_000),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(firmware.generation, Generation::V4);
    assert_eq!(firmware.build, Build::Lite);
    assert_eq!(firmware.version, semver::Version::new(6, 1, 0));
    assert_eq!(firmware.can_frequency, Some(1_000_000));

    assert!(
        identify(&probe("mcu", None, "CARTOGRAPHER V3", "stm32f042x6", None))
            .unwrap()
            .is_err()
    );
    assert!(
        identify(&probe("mcu", None, "CARTOGRAPHER 6.1.0", "rp2040", None))
            .unwrap()
            .is_err()
    );
}

#[test]
fn reads_the_identify_dictionary_an_image_embeds() {
    let identity = image_identity(&application(
        "CARTOGRAPHER V4 6.1.0 lite",
        "stm32g431xx",
        Some(500_000),
    ))
    .unwrap();

    assert_eq!(identity.version, "CARTOGRAPHER V4 6.1.0 lite");
    assert_eq!(identity.app.as_deref(), Some("Cartographer"));
    assert_eq!(identity.mcu, "stm32g431xx");
    assert_eq!(identity.can_frequency, Some(500_000));
    assert_eq!(image_identity(&[0x78; 64]), None);
}

#[test]
fn refuses_an_image_for_another_mcu_link_or_flash_offset() {
    let running = identify(&probe(
        "mcu cartographer",
        Some("Cartographer"),
        "CARTOGRAPHER V4 6.1.0",
        "stm32g431xx",
        Some(1_000_000),
    ))
    .unwrap()
    .unwrap();

    assert!(
        check_image(
            &application("CARTOGRAPHER V4 6.2.0", "stm32g431xx", Some(1_000_000)),
            &running
        )
        .is_ok()
    );
    assert_eq!(
        check_image(
            &application("CARTOGRAPHER V4 6.2.0", "stm32g431xx", Some(500_000)),
            &running
        ),
        Err(Mismatch::Link {
            image: Some(500_000),
            probe: Some(1_000_000)
        })
    );
    let usb = check_image(
        &application("CARTOGRAPHER V4 6.2.0", "stm32g431xx", None),
        &running,
    )
    .unwrap_err();
    assert_eq!(
        usb.to_string(),
        "it is built for USB, and the probe runs CAN at 1000000 bit/s"
    );
    assert!(matches!(
        check_image(
            &application("CARTOGRAPHER V3 6.1.0", "stm32f042x6", Some(1_000_000)),
            &running
        ),
        Err(Mismatch::Mcu { .. })
    ));
    assert_eq!(
        check_image(
            &image(
                "CARTOGRAPHER V4 6.2.0",
                "stm32g431xx",
                Some(1_000_000),
                0x101
            ),
            &running
        ),
        Err(Mismatch::NotAnApplication),
        "an image with Katapult at the start of flash is not an update image"
    );
    assert_eq!(check_image(&[0; 512], &running), Err(Mismatch::NoIdentity));
}

#[test]
fn keeps_only_update_images_from_the_manifest() {
    let manifest = parse_manifest(&format!(
        "{HEADER}\r\n\
         v4/firmware/6.2.0/a.bin,a.bin,Cartographer,CAN,Update,v4,6.2.0,1000000,No,1.6.0\r\n\
         v4/combined/6.2.0/b.bin,b.bin,Cartographer + Katapult,USB,DFU,v4,6.2.0,,No,\r\n\
         v4/firmware/6.1.0/c.bin,c.bin,Cartographer,USB,Update,v4,6.1.0,,Yes,\r\n\
         v4/firmware/x/d.bin,d.bin,Cartographer,USB,Update,v4,not-a-version,,No,\r\n"
    ))
    .unwrap();

    assert_eq!(manifest.len(), 2);
    assert_eq!(manifest[0].path, "v4/firmware/6.2.0/a.bin");
    assert_eq!(manifest[0].can_speed, Some(1_000_000));
    assert!(!manifest[0].lite);
    assert_eq!(manifest[0].min_plugin, Some(semver::Version::new(1, 6, 0)));
    assert_eq!(manifest[1].protocol, "USB");
    assert!(manifest[1].lite);
    assert_eq!(manifest[1].min_plugin, None);

    assert!(parse_manifest("filepath,protocol\n").is_err());
}

struct Clone {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    rows: Vec<String>,
}

impl Clone {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cartographer_firmware");
        std::fs::create_dir_all(root.join("firmware")).unwrap();
        Self {
            _dir: dir,
            root,
            rows: Vec::new(),
        }
    }

    /// Lists `image` in the manifest with these columns and writes it at `path`.
    fn add(&mut self, path: &str, columns: &str, image: &[u8]) -> &mut Self {
        let file = self.root.join("firmware").join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, image).unwrap();
        let name = Path::new(path).file_name().unwrap().to_string_lossy();
        self.rows
            .push(format!("{path},{name},Cartographer,{columns}"));
        self
    }

    fn write(&self) -> &Path {
        let mut manifest = format!("{HEADER}\r\n");
        for row in &self.rows {
            manifest.push_str(row);
            manifest.push_str("\r\n");
        }
        std::fs::write(self.root.join("firmware_list.csv"), manifest).unwrap();
        &self.root
    }
}

fn v4_clone() -> Clone {
    let mut clone = Clone::new();
    clone
        .add(
            "v4/firmware/6.1.0/usb_full.bin",
            "USB,Update,v4,6.1.0,,No,",
            &application("CARTOGRAPHER V4 6.1.0", "stm32g431xx", None),
        )
        .add(
            "v4/firmware/6.1.0/usb_lite.bin",
            "USB,Update,v4,6.1.0,,Yes,",
            &application("CARTOGRAPHER V4 6.1.0 lite", "stm32g431xx", None),
        )
        .add(
            "v4/firmware/6.1.0/can_500k.bin",
            "CAN,Update,v4,6.1.0,500000,No,",
            &application("CARTOGRAPHER V4 6.1.0", "stm32g431xx", Some(500_000)),
        )
        .add(
            "v4/firmware/6.2.0/usb_full.bin",
            "USB,Update,v4,6.2.0,,No,1.6.0",
            &application("CARTOGRAPHER v4 6.2.0", "stm32g431xx", None),
        );
    clone
}

fn targets(
    clone: &Path,
    mcus: Vec<Mcu>,
    plugin: Option<(u64, u64, u64)>,
) -> aldis::cartographer::Targets {
    resolve_targets(
        &McuInventory {
            mcus,
            unreported: Vec::new(),
        },
        Some(clone),
        plugin.map(|(major, minor, patch)| semver::Version::new(major, minor, patch)),
    )
}

#[test]
fn picks_the_newest_image_the_installed_plugin_works_with() {
    let clone = v4_clone();
    let root = clone.write();
    let usb = probe(
        "mcu cartographer",
        Some("Cartographer"),
        "CARTOGRAPHER V4 6.0.0",
        "stm32g431xx",
        None,
    );

    let current = targets(root, vec![usb.clone()], Some((1, 6, 0)));
    assert_eq!(
        current.target("mcu cartographer"),
        Some(&Target {
            path: root.join("firmware/v4/firmware/6.2.0/usb_full.bin"),
            version: semver::Version::new(6, 2, 0),
            reported: "CARTOGRAPHER v4 6.2.0".to_owned(),
            held_back: None,
        })
    );

    let old_plugin = targets(root, vec![usb], Some((1, 5, 3)));
    let target = old_plugin.target("mcu cartographer").unwrap();
    assert_eq!(target.version, semver::Version::new(6, 1, 0));
    assert_eq!(
        target.held_back,
        Some((semver::Version::new(6, 2, 0), semver::Version::new(1, 6, 0)))
    );
    assert_eq!(old_plugin.plugin, Some(semver::Version::new(1, 5, 3)));
}

#[test]
fn matches_the_build_and_can_bitrate_the_probe_runs() {
    let clone = v4_clone();
    let root = clone.write();

    let lite = targets(
        root,
        vec![probe(
            "mcu cartographer",
            Some("Cartographer"),
            "CARTOGRAPHER V4 6.0.0 Lite",
            "stm32g431xx",
            None,
        )],
        Some((1, 6, 0)),
    );
    assert_eq!(
        lite.target("mcu cartographer").unwrap().reported,
        "CARTOGRAPHER V4 6.1.0 lite"
    );

    let can = targets(
        root,
        vec![
            probe(
                "mcu cartographer",
                None,
                "CARTOGRAPHER V4 6.0.0",
                "stm32g431xx",
                Some(500_000),
            ),
            probe(
                "mcu scanner",
                None,
                "CARTOGRAPHER V4 6.0.0",
                "stm32g431xx",
                Some(1_000_000),
            ),
        ],
        Some((1, 6, 0)),
    );
    assert_eq!(
        can.target("mcu cartographer").unwrap().path,
        root.join("firmware/v4/firmware/6.1.0/can_500k.bin")
    );
    assert_eq!(
        can.mcus["mcu scanner"],
        Err(Unavailable::NoImage(
            "v4 CAN at 1000000 bit/s full build".to_owned()
        ))
    );
}

#[test]
fn skips_a_listed_image_whose_own_dictionary_disagrees_with_the_manifest() {
    let mut clone = Clone::new();
    clone
        .add(
            "v4/firmware/6.1.0/mislabelled.bin",
            "CAN,Update,v4,6.1.0,1000000,No,",
            &application("CARTOGRAPHER V4 6.1.0", "stm32g431xx", Some(500_000)),
        )
        .add(
            "v4/firmware/6.1.0/lite_as_full.bin",
            "USB,Update,v4,6.1.0,,No,",
            &application("CARTOGRAPHER V4 6.1.0 lite", "stm32g431xx", None),
        )
        .add(
            "v4/firmware/6.1.0/wrong_version.bin",
            "USB,Update,v4,6.1.0,,No,",
            &application("CARTOGRAPHER V4 6.0.0", "stm32g431xx", None),
        )
        .add(
            "v4/firmware/6.1.0/combined.bin",
            "USB,Update,v4,6.1.0,,No,",
            &image("CARTOGRAPHER V4 6.1.0", "stm32g431xx", None, 0x101),
        );
    let root = clone.write();

    let found = targets(
        root,
        vec![
            probe(
                "mcu can",
                None,
                "CARTOGRAPHER V4 6.0.0",
                "stm32g431xx",
                Some(1_000_000),
            ),
            probe(
                "mcu usb",
                None,
                "CARTOGRAPHER V4 6.0.0",
                "stm32g431xx",
                None,
            ),
        ],
        None,
    );

    assert!(matches!(
        found.mcus["mcu can"],
        Err(Unavailable::NoImage(_))
    ));
    assert!(matches!(
        found.mcus["mcu usb"],
        Err(Unavailable::NoImage(_))
    ));
}

#[test]
fn refuses_several_images_at_the_chosen_version() {
    let mut clone = Clone::new();
    for name in ["a", "b"] {
        clone.add(
            &format!("v4/firmware/6.1.0/{name}.bin"),
            "USB,Update,v4,6.1.0,,No,",
            &application("CARTOGRAPHER V4 6.1.0", "stm32g431xx", None),
        );
    }
    let root = clone.write();

    let found = targets(
        root,
        vec![probe(
            "mcu cartographer",
            None,
            "CARTOGRAPHER V4 6.0.0",
            "stm32g431xx",
            None,
        )],
        None,
    );

    assert!(matches!(
        &found.mcus["mcu cartographer"],
        Err(Unavailable::Ambiguous(paths)) if paths.len() == 2
    ));
}

#[test]
fn explains_an_image_only_a_newer_plugin_works_with() {
    let mut clone = Clone::new();
    clone.add(
        "v4/firmware/6.2.0/usb_full.bin",
        "USB,Update,v4,6.2.0,,No,1.6.0",
        &application("CARTOGRAPHER v4 6.2.0", "stm32g431xx", None),
    );
    let root = clone.write();
    let probe = || {
        probe(
            "mcu cartographer",
            None,
            "CARTOGRAPHER V4 6.1.0",
            "stm32g431xx",
            None,
        )
    };

    let error = targets(root, vec![probe()], None).mcus["mcu cartographer"]
        .clone()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "every matching image needs cartographer3d-plugin 1.6.0 or newer, and no installed \
         version was found"
    );
    let older = targets(root, vec![probe()], Some((1, 5, 0))).mcus["mcu cartographer"]
        .clone()
        .unwrap_err();
    assert!(
        older.to_string().ends_with("and 1.5.0 is installed"),
        "{older}"
    );
}

#[test]
fn refuses_a_manifest_path_outside_the_firmware_directory() {
    let mut clone = Clone::new();
    let outside = clone.root.join("outside.bin");
    std::fs::write(
        &outside,
        application("CARTOGRAPHER V4 6.1.0", "stm32g431xx", None),
    )
    .unwrap();
    clone
        .rows
        .push("../outside.bin,outside.bin,Cartographer,USB,Update,v4,6.1.0,,No,".to_owned());
    let root = clone.write();

    let found = targets(
        root,
        vec![probe(
            "mcu cartographer",
            None,
            "CARTOGRAPHER V4 6.0.0",
            "stm32g431xx",
            None,
        )],
        None,
    );

    assert!(matches!(
        found.mcus["mcu cartographer"],
        Err(Unavailable::NoImage(_))
    ));
}

#[test]
fn needs_a_configured_link_that_matches_what_the_probe_reports() {
    let clone = v4_clone();
    let root = clone.write();
    let mut unconfigured = probe("mcu a", None, "CARTOGRAPHER V4 6.0.0", "stm32g431xx", None);
    unconfigured.transport = None;
    let mut can_without_bitrate = probe(
        "mcu b",
        None,
        "CARTOGRAPHER V4 6.0.0",
        "stm32g431xx",
        Some(500_000),
    );
    can_without_bitrate.canbus_frequency_hz = None;
    let mut serial_with_bitrate =
        probe("mcu c", None, "CARTOGRAPHER V4 6.0.0", "stm32g431xx", None);
    serial_with_bitrate.canbus_frequency_hz = Some(500_000);

    let found = targets(
        root,
        vec![unconfigured, can_without_bitrate, serial_with_bitrate],
        None,
    );

    assert_eq!(found.mcus["mcu a"], Err(Unavailable::NotConfigured));
    assert_eq!(found.mcus["mcu b"], Err(Unavailable::NoCanBitrate));
    assert!(matches!(found.mcus["mcu c"], Err(Unavailable::NotCan(_))));
}

#[test]
fn reports_a_missing_clone_or_manifest() {
    let mcus = || {
        vec![probe(
            "mcu cartographer",
            None,
            "CARTOGRAPHER V4 6.0.0",
            "stm32g431xx",
            None,
        )]
    };
    let none = resolve_targets(
        &McuInventory {
            mcus: mcus(),
            unreported: Vec::new(),
        },
        None,
        None,
    );
    assert_eq!(none.mcus["mcu cartographer"], Err(Unavailable::NoCheckout));

    let dir = tempfile::tempdir().unwrap();
    let broken = targets(dir.path(), mcus(), None);
    assert!(matches!(
        broken.mcus["mcu cartographer"],
        Err(Unavailable::Manifest { .. })
    ));
}

#[test]
fn does_not_downgrade_a_probe_newer_than_its_image() {
    let running = |version: &str| {
        identify(&probe("mcu", None, version, "stm32g431xx", None))
            .unwrap()
            .unwrap()
    };
    let target = Target {
        path: "/x.bin".into(),
        version: semver::Version::new(6, 1, 0),
        reported: "CARTOGRAPHER V4 6.1.0".to_owned(),
        held_back: None,
    };

    assert_eq!(
        compare(&running("CARTOGRAPHER V4 6.0.0"), &target),
        RevisionStatus::UpdateRequired
    );
    assert_eq!(
        compare(&running("CARTOGRAPHER V4 6.1.0"), &target),
        RevisionStatus::Current
    );
    assert_eq!(
        compare(&running("CARTOGRAPHER v4 6.2.0"), &target),
        RevisionStatus::Indeterminate
    );
}

#[test]
fn finds_the_clone_and_the_installed_plugin_version() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    assert_eq!(locate_checkout(None, Some(home)), None);
    let clone = home.join("cartographer_firmware");
    std::fs::create_dir_all(&clone).unwrap();
    std::fs::write(clone.join("firmware_list.csv"), HEADER).unwrap();
    assert_eq!(locate_checkout(None, Some(home)), Some(clone.clone()));
    assert_eq!(locate_checkout(Some(home), Some(home)), None);

    let venv = home.join("klippy-env");
    assert_eq!(plugin_version_in(&venv), None);
    let packages = venv.join("lib/python3.11/site-packages");
    std::fs::create_dir_all(packages.join("cartographer3d_plugin-1.5.3b2.dev1.dist-info")).unwrap();
    assert_eq!(
        plugin_version_in(&venv),
        Some(semver::Version::new(1, 5, 3))
    );
}

#[test]
fn starts_cartographer_applications_above_its_katapult() {
    assert_eq!(aldis::cartographer::APPLICATION_START, 0x0800_2000);
}

#[test]
fn refuses_an_image_that_is_not_cartographer_firmware() {
    let running = identify(&probe(
        "mcu cartographer",
        Some("Cartographer"),
        "CARTOGRAPHER V4 6.1.0",
        "stm32g431xx",
        Some(1_000_000),
    ))
    .unwrap()
    .unwrap();
    let klipper = image_of(
        "Klipper",
        "v0.13.0-770-gce7002bed",
        "stm32g431xx",
        Some(1_000_000),
        0x2100,
    );

    assert_eq!(
        check_image(&klipper, &running),
        Err(Mismatch::NotCartographer)
    );
    assert_eq!(
        Mismatch::NotCartographer.to_string(),
        "it is not Cartographer firmware"
    );
    // Before 6.1.0 a Cartographer names itself only in its version, with no app.
    assert!(
        check_image(
            &image_of(
                "",
                "CARTOGRAPHER V4 5.1.0",
                "stm32g431xx",
                Some(1_000_000),
                0x2100
            ),
            &running
        )
        .is_ok()
    );
}
