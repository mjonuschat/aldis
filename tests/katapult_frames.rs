use mcu_update::flash::katapult::{Command, FrameError, crc16_ccitt, encode_command};

#[test]
fn computes_the_crc_used_by_katapult_frames() {
    assert_eq!(crc16_ccitt(b"123456789"), 0x6f91);
}

#[test]
fn encodes_a_complete_command_with_little_endian_crc() {
    assert_eq!(
        encode_command(Command::Complete, &[]),
        Ok(vec![0x01, 0x88, 0x15, 0x00, 0x91, 0x1b, 0x99, 0x03])
    );
}

#[test]
fn rejects_payloads_that_cannot_be_represented_as_words() {
    assert_eq!(
        encode_command(Command::SendBlock, &[0; 3]),
        Err(FrameError::PayloadNotWordAligned { length: 3 })
    );
}

#[test]
fn rejects_payloads_larger_than_the_one_byte_word_count() {
    assert_eq!(
        encode_command(Command::SendBlock, &[0; 1024]),
        Err(FrameError::PayloadTooLarge { length: 1024 })
    );
}
