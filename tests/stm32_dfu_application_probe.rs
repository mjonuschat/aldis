use aldis::flash::stm32_dfu::{ApplicationProbeResult, probe_application_start};

#[test]
fn finds_the_sole_plausible_vector_table_among_candidate_offsets() {
    let result = probe_application_start(|address| {
        if address == 0x0800_8000 {
            Some(vector_table_bytes(0x2000_1000, 0x0800_8101))
        } else {
            Some([0xff; 8])
        }
    });

    assert_eq!(result, ApplicationProbeResult::Found(0x0800_8000));
}

#[test]
fn reports_ambiguous_when_more_than_one_offset_looks_valid() {
    let result =
        probe_application_start(|address| Some(vector_table_bytes(0x2000_1000, address | 1)));

    assert!(matches!(result, ApplicationProbeResult::Ambiguous(matches) if matches.len() > 1));
}

#[test]
fn reports_not_found_when_nothing_looks_like_a_valid_vector_table() {
    let result = probe_application_start(|_| Some([0xff; 8]));

    assert_eq!(result, ApplicationProbeResult::NotFound);
}

#[test]
fn skips_offsets_the_reader_could_not_access() {
    let result = probe_application_start(|address| {
        (address == 0x0800_8000).then(|| vector_table_bytes(0x2000_1000, 0x0800_8101))
    });

    assert_eq!(result, ApplicationProbeResult::Found(0x0800_8000));
}

fn vector_table_bytes(stack_pointer: u32, reset_vector: u32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&stack_pointer.to_le_bytes());
    bytes[4..8].copy_from_slice(&reset_vector.to_le_bytes());
    bytes
}
