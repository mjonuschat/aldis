use mcu_update::moonraker::parse_inventory;
use mcu_update::plan::{UpdateStep, build_update_plan};

#[test]
fn plans_sequential_updates_without_changing_service_state() {
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");

    let plan = build_update_plan(&inventory);

    assert!(plan.klipper.runs_during_discovery);
    assert!(plan.klipper.stops_before_first_build_or_flash);
    assert!(!plan.klipper.restarts_automatically);
    assert_eq!(plan.targets.len(), 2);
    assert_eq!(plan.targets[0].name, "mcu");
    assert_eq!(plan.targets[1].name, "mcu toolhead");
    assert_eq!(
        plan.targets[1].steps,
        vec![
            UpdateStep::ValidateConfiguration,
            UpdateStep::ConfirmWrite,
            UpdateStep::BuildFirmware,
            UpdateStep::EnterBootloader,
            UpdateStep::FlashFirmware,
            UpdateStep::VerifyFirmware,
            UpdateStep::CompleteBootloader,
        ]
    );
}
