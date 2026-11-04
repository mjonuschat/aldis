use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use aldis::identify::{
    FrameDecoder, IdentifyData, IdentifyError, MAX_DICTIONARY_BYTES, MAX_INFLATED_BYTES,
    decode_vlq, encode_frame, encode_vlq, identify_over, identify_request, parse_identify_response,
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
    completion_pending: bool,
    completion_delay: Option<Duration>,
}

impl FakeMcu {
    fn new(expected_seq: u8, dictionary: Vec<u8>) -> Self {
        Self {
            expected_seq,
            dictionary,
            pending: VecDeque::new(),
            decoder: FrameDecoder::new(),
            completion_pending: false,
            completion_delay: None,
        }
    }
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
            self.completion_pending = chunk.is_empty();
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
        if self.completion_pending
            && let Some(delay) = self.completion_delay.take()
        {
            std::thread::sleep(delay);
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
    let mut mcu = FakeMcu::new(7, dictionary());

    let data = identify_over(&mut mcu, Duration::from_secs(2)).expect("identify");

    assert_eq!(data, dictionary());
}

#[test]
fn accepts_compressed_dictionaries_at_the_limit_and_rejects_overflow() {
    for size in [
        MAX_DICTIONARY_BYTES - 1,
        MAX_DICTIONARY_BYTES,
        MAX_DICTIONARY_BYTES + 1,
    ] {
        let mut mcu = FakeMcu::new(0, vec![b'x'; size]);
        let result = identify_over(&mut mcu, Duration::from_secs(60));
        if size > MAX_DICTIONARY_BYTES {
            assert!(
                matches!(result, Err(IdentifyError::DictionaryTooLarge(limit)) if limit == MAX_DICTIONARY_BYTES as u64)
            );
        } else {
            assert_eq!(result.expect("dictionary within the cap").len(), size);
        }
    }
}

#[test]
fn refuses_a_terminating_reply_received_after_the_deadline() {
    let timeout = Duration::from_millis(100);
    let mut mcu = FakeMcu::new(0, dictionary());
    mcu.completion_delay = Some(timeout * 2);

    let result = identify_over(&mut mcu, timeout);

    assert!(
        mcu.completion_delay.is_none(),
        "the terminating read must be delayed"
    );
    assert!(matches!(result, Err(IdentifyError::Timeout(duration)) if duration == timeout));
}

/// Answers every request with a full chunk at the requested offset, forever.
struct EndlessMcu {
    pending: VecDeque<u8>,
    decoder: FrameDecoder,
}

impl Write for EndlessMcu {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.decoder.push(bytes);
        while let Some(frame) = self.decoder.next_frame() {
            let mut pos = 1;
            let offset = decode_vlq(&frame.payload, &mut pos).unwrap();
            let count = decode_vlq(&frame.payload, &mut pos).unwrap() as u8;
            let mut payload = vec![0x00];
            encode_vlq(&mut payload, offset);
            payload.push(count);
            payload.extend(std::iter::repeat_n(0xaa, usize::from(count)));
            self.pending
                .extend(encode_frame(frame.seq & 0x0f, &payload));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for EndlessMcu {
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
fn stops_once_the_dictionary_exceeds_the_size_cap() {
    let mut mcu = EndlessMcu {
        pending: VecDeque::new(),
        decoder: FrameDecoder::new(),
    };

    let error = identify_over(&mut mcu, Duration::from_secs(60)).unwrap_err();

    assert!(
        matches!(error, IdentifyError::DictionaryTooLarge(limit) if limit == MAX_DICTIONARY_BYTES as u64),
        "{error:?}"
    );
}

/// Never blocks: every read returns valid frames that are not identify responses.
struct FloodingMcu;

impl Write for FloodingMcu {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for FloodingMcu {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let frame = encode_frame(0, &[]);
        let n = out.len().min(frame.len());
        out[..n].copy_from_slice(&frame[..n]);
        Ok(n)
    }
}

#[test]
fn gives_up_on_an_mcu_that_floods_frames_without_ever_answering() {
    let started = Instant::now();

    let error = identify_over(&mut FloodingMcu, Duration::from_millis(50)).unwrap_err();

    assert!(
        matches!(error, IdentifyError::Timeout(timeout) if timeout == Duration::from_millis(50)),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn reports_timeout_after_a_reply_even_when_it_expires_during_a_write() {
    struct PausingMcu {
        requests: usize,
    }
    impl Write for PausingMcu {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > 1 {
                self.requests += 1;
                if self.requests == 2 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Read for PausingMcu {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            FloodingMcu.read(out)
        }
    }
    let timeout = Duration::from_millis(100);
    let mut mcu = PausingMcu { requests: 0 };

    let result = identify_over(&mut mcu, timeout);

    assert_eq!(mcu.requests, 2, "a reply must precede the delayed write");
    assert!(matches!(result, Err(IdentifyError::Timeout(duration)) if duration == timeout));
}

#[test]
fn accepts_valid_json_at_the_inflated_limit_and_rejects_overflow() {
    for size in [
        MAX_INFLATED_BYTES - 1,
        MAX_INFLATED_BYTES,
        MAX_INFLATED_BYTES + 1,
    ] {
        let mut json = br#"{"version":"v","config":{"MCU":"test"}}"#.to_vec();
        json.resize(size as usize, b' ');
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&json).unwrap();
        let result = IdentifyData::from_compressed(&encoder.finish().unwrap());
        if size > MAX_INFLATED_BYTES {
            assert!(
                matches!(result, Err(IdentifyError::DictionaryTooLarge(limit)) if limit == MAX_INFLATED_BYTES)
            );
        } else {
            assert_eq!(result.expect("JSON within the cap").version, "v");
        }
    }
}

#[test]
fn refuses_a_dictionary_that_inflates_past_the_cap() {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let spaces = vec![b' '; 1024 * 1024];
    for _ in 0..9 {
        encoder.write_all(&spaces).unwrap();
    }
    let bomb = encoder.finish().unwrap();
    assert!(
        bomb.len() < 64 * 1024,
        "the fixture must be small on the wire"
    );

    let error = IdentifyData::from_compressed(&bomb).unwrap_err();

    assert!(
        matches!(error, IdentifyError::DictionaryTooLarge(limit) if limit == MAX_INFLATED_BYTES),
        "{error:?}"
    );
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
