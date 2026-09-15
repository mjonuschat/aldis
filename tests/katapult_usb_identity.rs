use aldis::flash::katapult::serial::is_katapult_usb;

#[test]
fn recognizes_katapult_by_its_usb_id_or_manufacturer() {
    assert!(is_katapult_usb("1D50:6177", "unknown"));
    assert!(is_katapult_usb("other", "Katapult"));
    assert!(!is_katapult_usb("1d50:614e", "klipper"));
}
