use aldis::flash::stm32_dfu::{
    expected_application_start, plausible_application_starts, select_application_start,
};

fn image_with_vector_table(stack_pointer: u32, reset_vector: u32) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(&stack_pointer.to_le_bytes());
    image.extend_from_slice(&reset_vector.to_le_bytes());
    image.extend_from_slice(&[0xAA; 32]);
    image
}

#[test]
fn matches_the_candidate_region_the_reset_vector_falls_within() {
    let image = image_with_vector_table(0x2000_1000, 0x0800_20C1);
    assert_eq!(expected_application_start(&image), Some(0x0800_2000));
}

#[test]
fn rounds_down_to_the_nearest_candidate_below_a_reset_vector_in_a_gap() {
    let image = image_with_vector_table(0x2000_1000, 0x0800_9501);
    assert_eq!(expected_application_start(&image), Some(0x0800_9000));
}

#[test]
fn matches_the_zero_offset_candidate_at_the_base_of_flash() {
    let image = image_with_vector_table(0x2000_1000, 0x0800_00C1);
    assert_eq!(expected_application_start(&image), Some(0x0800_0000));
}

#[test]
fn rejects_an_implausible_stack_pointer_outside_sram() {
    let image = image_with_vector_table(0x0800_1000, 0x0800_20C1);
    assert_eq!(expected_application_start(&image), None);
}

#[test]
fn rejects_firmware_shorter_than_one_vector_table() {
    assert_eq!(expected_application_start(&[0u8; 4]), None);
}

fn image_with_handlers(handlers: &[u32], length: usize) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(&0x2002_4000_u32.to_le_bytes());
    for handler in handlers {
        image.extend_from_slice(&handler.to_le_bytes());
    }
    image.resize(length, 0xAA);
    image
}

#[test]
fn names_every_start_consistent_with_startup_code_far_into_the_image() {
    let image = image_with_handlers(&[0x0800_7765, 0x0800_7779, 0x0800_777D], 0xAF14);
    let starts = plausible_application_starts(&image);

    assert!(starts.contains(&0x0800_0000));
    assert!(starts.contains(&0x0800_7000));
    assert!(!starts.contains(&0x0800_8000));
    assert_eq!(expected_application_start(&image), Some(0x0800_7000));
}

#[test]
fn names_a_single_start_when_the_handlers_sit_near_the_image_start() {
    let image = image_with_handlers(&[0x0800_20C1, 0x0800_20C5], 0x200);
    assert_eq!(plausible_application_starts(&image), vec![0x0800_2000]);
}

#[test]
fn names_no_start_for_an_implausible_vector_table() {
    let image = image_with_handlers(&[0x0800_20C0], 0x40);
    assert!(plausible_application_starts(&image).is_empty());
}

#[test]
fn prefers_the_installed_application_then_the_configured_one() {
    let plausible = [0x0800_0000, 0x0800_7000];

    assert_eq!(
        select_application_start(&plausible, Some(0x0800_0000), Some(0x0800_7000)),
        Some(0x0800_0000)
    );
    assert_eq!(
        select_application_start(&plausible, None, Some(0x0800_7000)),
        Some(0x0800_7000)
    );
    assert_eq!(
        select_application_start(&plausible, Some(0x0802_0000), Some(0x0800_0000)),
        Some(0x0800_0000)
    );
    assert_eq!(
        select_application_start(&plausible, Some(0x0802_0000), None),
        None
    );
}
