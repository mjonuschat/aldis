use aldis::host::{HostCheckError, check_host, is_loopback_url};
use aldis::moonraker::parse_klipper_unit;

#[test]
fn treats_only_loopback_hosts_as_local() {
    for url in [
        "http://127.0.0.1:7125",
        "http://127.0.0.2:7125",
        "http://localhost:7125",
        "http://LOCALHOST",
        "http://[::1]:7125",
    ] {
        assert!(is_loopback_url(url), "{url}");
    }
    for url in [
        "http://192.168.1.20:7125",
        "http://printer.local:7125",
        "http://[fe80::1]:7125",
        "not a url",
    ] {
        assert!(!is_loopback_url(url), "{url}");
    }
}

#[test]
fn rejects_a_remote_moonraker_before_checking_the_instance() {
    assert_eq!(
        check_host("http://192.168.1.20:7125", Some("klipper")),
        Err(HostCheckError::NonLocal(
            "http://192.168.1.20:7125".to_owned()
        ))
    );
}

#[test]
fn rejects_a_non_default_klipper_unit_and_accepts_default_or_unknown() {
    assert_eq!(
        check_host("http://127.0.0.1:7125", Some("klipper-1")),
        Err(HostCheckError::UnsupportedInstance("klipper-1".to_owned()))
    );
    assert_eq!(check_host("http://127.0.0.1:7125", Some("klipper")), Ok(()));
    assert_eq!(check_host("http://127.0.0.1:7125", None), Ok(()));
}

#[test]
fn refuses_when_moonraker_cannot_report_the_unit() {
    use aldis::host::verify_host;
    use aldis::moonraker::{HostPort, MoonrakerError};

    struct Broken;
    impl HostPort for Broken {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Err(MoonrakerError::KlippyNotConnected)
        }
    }

    assert!(matches!(
        verify_host("http://127.0.0.1:7125", &Broken),
        Err(HostCheckError::Unavailable(_))
    ));
}

#[test]
fn reads_the_klipper_unit_from_system_info() {
    let response = r#"{"result":{"system_info":{"instance_ids":{"moonraker":"moonraker","klipper":"klipper-2"}}}}"#;
    assert_eq!(
        parse_klipper_unit(response).unwrap(),
        Some("klipper-2".to_owned())
    );
    let older = r#"{"result":{"system_info":{}}}"#;
    assert_eq!(parse_klipper_unit(older).unwrap(), None);
}
