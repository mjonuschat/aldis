use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::Duration;

use aldis::identify::{
    FrameDecoder, IdentifyData, IdentifyError, decode_vlq, encode_frame, encode_vlq, identify_over,
    identify_request, parse_identify_response,
};

#[test]
fn encodes_integers_like_klipper() {
    for (value, expected) in [
        (0u32, vec![0x00]),
        (40, vec![0x28]),
        (0x60, vec![0x80, 0x60]),
        (200, vec![0x81, 0x48]),
        (100_000, vec![0x86, 0x8d, 0x20]),
    ] {
        let mut out = Vec::new();
        encode_vlq(&mut out, value);
        assert_eq!(out, expected, "{value}");
        let mut pos = 0;
        assert_eq!(decode_vlq(&out, &mut pos), Some(value));
        assert_eq!(pos, out.len());
    }
}

#[test]
fn builds_the_identify_request_frame_klipper_sends() {
    assert_eq!(
        identify_request(0, 0, 40),
        vec![0x08, 0x10, 0x01, 0x00, 0x28, 0x5e, 0x9f, 0x7e]
    );
}

#[test]
fn decodes_a_response_frame_after_leading_garbage() {
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0xff, 0x00]);
    decoder.push(&[
        0x0b, 0x11, 0x00, 0x00, 0x03, b'a', b'b', b'c', 0x8c, 0xd8, 0x7e,
    ]);

    let frame = decoder.next_frame().expect("frame");

    assert_eq!(frame.seq, 0x11);
    assert_eq!(
        parse_identify_response(&frame.payload),
        Some((0, b"abc".to_vec()))
    );
    assert!(decoder.next_frame().is_none());
}

#[test]
fn drops_a_frame_with_a_bad_crc() {
    let mut decoder = FrameDecoder::new();
    decoder.push(&[
        0x0b, 0x11, 0x00, 0x00, 0x03, b'a', b'b', b'c', 0x00, 0x00, 0x7e,
    ]);
    assert!(decoder.next_frame().is_none());
}

fn dictionary() -> Vec<u8> {
    let json = serde_json::json!({
        "version": "v0.13.0-770-gce7002bed",
        "build_versions": "gcc: 12",
        "config": {"MCU": "samd21g18a", "CLOCK_FREQ": 48000000},
        "kconfig": "CONFIG_MACH_ATSAMD=y\n",
        "commands": {}, "responses": {}
    });
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(json.to_string().as_bytes()).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn decodes_the_compressed_dictionary() {
    let data = IdentifyData::from_compressed(&dictionary()).unwrap();

    assert_eq!(data.version, "v0.13.0-770-gce7002bed");
    assert_eq!(data.mcu, "samd21g18a");
    assert_eq!(data.kconfig, "CONFIG_MACH_ATSAMD=y\n");
    assert_eq!(data.canbus_frequency_hz, None);
    assert_eq!(data.app, None);
}

/// Replies to each identify request from a scripted list; a request with the wrong sequence
/// number gets a NAK carrying the expected one, as Klipper firmware does.
struct FakeMcu {
    expected_seq: u8,
    dictionary: Vec<u8>,
    pending: VecDeque<u8>,
    decoder: FrameDecoder,
}

impl Write for FakeMcu {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.decoder.push(bytes);
        while let Some(frame) = self.decoder.next_frame() {
            if frame.seq & 0x0f != self.expected_seq {
                self.pending.extend(encode_frame(self.expected_seq, &[]));
                continue;
            }
            let mut pos = 1;
            let offset = decode_vlq(&frame.payload, &mut pos).unwrap() as usize;
            let count = decode_vlq(&frame.payload, &mut pos).unwrap() as usize;
            let end = (offset + count).min(self.dictionary.len());
            let chunk = &self.dictionary[offset.min(end)..end];
            self.expected_seq = (self.expected_seq + 1) & 0x0f;
            let mut payload = vec![0x00];
            encode_vlq(&mut payload, offset as u32);
            payload.push(chunk.len() as u8);
            payload.extend_from_slice(chunk);
            self.pending
                .extend(encode_frame(self.expected_seq, &payload));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for FakeMcu {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = out.len().min(self.pending.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.pending.pop_front().unwrap();
        }
        Ok(n)
    }
}

#[test]
fn reads_the_whole_dictionary_after_resyncing_from_a_nak() {
    let mut mcu = FakeMcu {
        expected_seq: 7,
        dictionary: dictionary(),
        pending: VecDeque::new(),
        decoder: FrameDecoder::new(),
    };

    let data = identify_over(&mut mcu, Duration::from_secs(2)).expect("identify");

    assert_eq!(data, dictionary());
}

#[test]
fn gives_up_when_nothing_answers() {
    struct Silent;
    impl Write for Silent {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::TimedOut.into())
        }
    }

    assert!(matches!(
        identify_over(&mut Silent, Duration::from_millis(50)),
        Err(IdentifyError::NoResponse)
    ));
}
