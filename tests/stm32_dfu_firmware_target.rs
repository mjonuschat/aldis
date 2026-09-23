use aldis::flash::stm32_dfu::expected_application_start;

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
