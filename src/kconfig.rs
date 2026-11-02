//! Validation of the Kconfig text an MCU reports before it is handed to `make`.
//!
//! The embedded Kconfig comes from whatever firmware is currently running on the board (via
//! Moonraker's `mcu_kconfig` or the MCU's own identify dictionary), so it is untrusted input.
//! Klipper's top-level `Makefile` does `-include $(KCONFIG_CONFIG)` and board Makefiles pass
//! values such as `CONFIG_MCU` through `$(shell ...)`, so any line that is not a plain Kconfig
//! setting would be evaluated by `make` (and possibly a shell) on the host before `olddefconfig`
//! ever normalizes it. This module admits only the line shapes Klipper's own Kconfig writes.

/// Why a reported Kconfig was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidKconfig {
    /// The text is larger than any real Klipper `.config`.
    #[error("the MCU-reported Kconfig is {0} bytes, larger than the {MAX_BYTES} aldis accepts")]
    TooLarge(usize),
    /// A line is not a Kconfig setting or comment.
    #[error(
        "line {line} of the MCU-reported Kconfig is not a plain Kconfig setting ({reason}): {text:?}"
    )]
    Line {
        /// 1-based line number.
        line: usize,
        /// What was wrong with it.
        reason: &'static str,
        /// The offending line, truncated for display.
        text: String,
    },
}

/// Larger than any Klipper `.config` by two orders of magnitude.
pub const MAX_BYTES: usize = 256 * 1024;
const MAX_LINE_LEN: usize = 1024;
const MAX_DISPLAY_LEN: usize = 80;

/// Checks that every line of `kconfig` is blank, a `#` comment (which covers
/// `# CONFIG_X is not set`), or `CONFIG_<symbol>=<value>` where the value is `y`, `n`, `m`, a
/// decimal or `0x` hexadecimal number, or a double-quoted string without characters that `make`
/// or a shell would interpret. `CONFIG_BOARD_DIRECTORY` must be a quoted single directory name
/// containing only ASCII letters, digits, underscores, or hyphens.
pub fn validate(kconfig: &str) -> Result<(), InvalidKconfig> {
    if kconfig.len() > MAX_BYTES {
        return Err(InvalidKconfig::TooLarge(kconfig.len()));
    }
    // `str::lines` would silently drop a `\r`; split on `\n` so a CR is caught as a control
    // character. A trailing newline leaves a final empty segment, which is a valid blank line.
    for (index, line) in kconfig.split('\n').enumerate() {
        if let Err(reason) = check_line(line) {
            return Err(InvalidKconfig::Line {
                line: index + 1,
                reason,
                text: truncated(line),
            });
        }
    }
    Ok(())
}

fn check_line(line: &str) -> Result<(), &'static str> {
    if line.len() > MAX_LINE_LEN {
        return Err("longer than 1024 characters");
    }
    if !line.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err("contains a control or non-ASCII character");
    }
    if line.trim().is_empty() {
        return Ok(());
    }
    if line.starts_with('#') {
        // A comment ending in a backslash continues onto the next line in make.
        return if line.contains('\\') {
            Err("comment contains a backslash")
        } else {
            Ok(())
        };
    }
    let Some((name, value)) = line.split_once('=') else {
        return Err("missing '='");
    };
    if !is_symbol(name) {
        return Err("not a CONFIG_ symbol assignment");
    }
    if !is_value(value) {
        return Err("value is not y, n, m, a number, or a safe quoted string");
    }
    // Make splits its board Makefile include on whitespace, without respecting quotes. Keep
    // this path component stricter than display strings so it cannot name extra include files.
    if name == "CONFIG_BOARD_DIRECTORY" && !is_board_directory(value) {
        return Err("CONFIG_BOARD_DIRECTORY is not a quoted single directory name");
    }
    Ok(())
}

fn is_board_directory(value: &str) -> bool {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .is_some_and(|directory| {
            !directory.is_empty()
                && directory
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
}

fn is_symbol(name: &str) -> bool {
    name.strip_prefix("CONFIG_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn is_value(value: &str) -> bool {
    matches!(value, "y" | "n" | "m") || is_number(value) || is_quoted_string(value)
}

fn is_number(value: &str) -> bool {
    if let Some(hex) = value.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `"..."` whose contents cannot end the quotes, expand in make (`$`), continue a line (`\`),
/// run or redirect in a shell (`` ` ; | & < > ``), glob (`* ?`), or escape the source tree
/// (`..`). Spaces and ordinary punctuation stay allowed so user-chosen strings such as a USB
/// product name or `CONFIG_INITIAL_PINS="!PA1,PB2"` pass.
fn is_quoted_string(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return false;
    };
    !inner.contains("..")
        && inner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" !#%'()+,-./:=@[]_".contains(&byte))
}

fn truncated(line: &str) -> String {
    if line.len() <= MAX_DISPLAY_LEN {
        line.to_owned()
    } else {
        let mut end = MAX_DISPLAY_LEN;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &line[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(kconfig: &str) -> (usize, &'static str) {
        match validate(kconfig).unwrap_err() {
            InvalidKconfig::Line { line, reason, .. } => (line, reason),
            other => panic!("expected a line error, got {other:?}"),
        }
    }

    #[test]
    fn accepts_what_klippers_kconfig_writes() {
        let kconfig = "#\n# Automatically generated file; DO NOT EDIT.\n# Klipper Firmware Configuration\n#\n\
                       CONFIG_LOW_LEVEL_OPTIONS=y\n# CONFIG_MACH_AVR is not set\nCONFIG_MACH_STM32=y\n\
                       CONFIG_MACH_atmega328p=n\nCONFIG_BOARD_DIRECTORY=\"stm32\"\nCONFIG_MCU=\"stm32h723xx\"\n\
                       CONFIG_CLOCK_FREQ=550000000\nCONFIG_FLASH_SIZE=0x100000\nCONFIG_FLASH_BOOT_ADDRESS=0x8020000\n\
                       CONFIG_USB_VENDOR_ID=0x1d50\nCONFIG_INITIAL_PINS=\"!PA1,PB2\"\nCONFIG_USB_PRODUCT=\"Voron 2.4 (Trident) #1\"\n\
                       CONFIG_INITIAL_PINS_EMPTY=\"\"\nCONFIG_OFFSET=-1\nCONFIG_RP2040_STAGE2_FILE=\"boot2_w25q080.S\"\n\n";
        assert_eq!(validate(kconfig), Ok(()));
        assert_eq!(validate(""), Ok(()));
        assert_eq!(validate("CONFIG_MACH_STM32=y"), Ok(()));
    }

    #[test]
    fn rejects_lines_make_would_evaluate() {
        assert_eq!(
            reason("CONFIG_MACH_STM32=y\nCROSS_PREFIX=/tmp/x/\n"),
            (2, "not a CONFIG_ symbol assignment")
        );
        assert_eq!(reason("include /tmp/evil.mk\n"), (1, "missing '='"));
        assert_eq!(
            reason("CONFIG_MCU=\"$(shell id > /tmp/pwned)\"\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\"stm32\"; id\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\"a\\\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\"a\" b\"\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\"`id`\"\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\"a|b\"\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=stm32\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("CONFIG_MCU=y # comment\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
    }

    #[test]
    fn rejects_make_assignment_operators_and_odd_symbol_names() {
        assert_eq!(
            reason("CONFIG_X!=id\n").1,
            "not a CONFIG_ symbol assignment"
        );
        assert_eq!(reason("CONFIG_X:=y\n").1, "not a CONFIG_ symbol assignment");
        assert_eq!(reason("CONFIG_X+=y\n").1, "not a CONFIG_ symbol assignment");
        assert_eq!(reason("CONFIG_=y\n").1, "not a CONFIG_ symbol assignment");
        assert_eq!(reason(" CONFIG_X=y\n").1, "not a CONFIG_ symbol assignment");
        assert_eq!(
            reason("CONFIG_X=y \n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(
            reason("export CONFIG_X=y\n").1,
            "not a CONFIG_ symbol assignment"
        );
    }

    #[test]
    fn rejects_source_tree_escapes_in_strings() {
        assert_eq!(
            reason("CONFIG_BOARD_DIRECTORY=\"../../tmp/evil\"\n").1,
            "value is not y, n, m, a number, or a safe quoted string"
        );
        assert_eq!(validate("CONFIG_SERIAL_PORT=\"/dev/ttyACM0\"\n"), Ok(()));
    }

    #[test]
    fn rejects_board_directory_include_injection() {
        for value in [
            "\"stm32 /tmp/uploaded.gcode tail\"",
            "\"/tmp/evil\"",
            "\"stm32/other\"",
            "\"stm32.mk\"",
            "\"\"",
            "y",
            "123",
        ] {
            let kconfig = format!("CONFIG_BOARD_DIRECTORY={value}\n");
            assert!(validate(&kconfig).is_err(), "accepted {kconfig:?}");
        }
        for directory in ["stm32", "rp2040", "linux", "hc32f460", "board_name-1"] {
            assert_eq!(
                validate(&format!("CONFIG_BOARD_DIRECTORY=\"{directory}\"\n")),
                Ok(())
            );
        }
    }

    #[test]
    fn truncates_rejected_non_ascii_at_a_character_boundary() {
        for character in ['\u{e9}', '\u{754c}', '\u{10000}'] {
            let line = format!("{}{character}", "a".repeat(79));
            match validate(&line).expect_err("non-ASCII input must be refused") {
                InvalidKconfig::Line { line, reason, text } => {
                    assert_eq!(line, 1);
                    assert_eq!(reason, "contains a control or non-ASCII character");
                    assert_eq!(text, format!("{}...", "a".repeat(79)));
                }
                other => panic!("expected a line error, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_comment_continuations_and_control_characters() {
        assert_eq!(
            reason("# hidden \\\nCONFIG_X=y\n"),
            (1, "comment contains a backslash")
        );
        assert_eq!(
            reason("CONFIG_X=y\r\n"),
            (1, "contains a control or non-ASCII character")
        );
        assert_eq!(
            reason("CONFIG_MCU=\"stm32\u{e9}\"\n"),
            (1, "contains a control or non-ASCII character")
        );
        assert_eq!(
            reason("CONFIG_X=y\n\tCONFIG_Y=y\n"),
            (2, "contains a control or non-ASCII character")
        );
    }

    #[test]
    fn rejects_oversized_input_and_truncates_long_lines_in_the_report() {
        let huge = "CONFIG_X=y\n".repeat(MAX_BYTES / 11 + 1);
        assert_eq!(validate(&huge), Err(InvalidKconfig::TooLarge(huge.len())));

        let long = format!("CONFIG_X=\"{}\"", "a".repeat(2000));
        match validate(&long).unwrap_err() {
            InvalidKconfig::Line { line, reason, text } => {
                assert_eq!((line, reason), (1, "longer than 1024 characters"));
                assert_eq!(text.len(), MAX_DISPLAY_LEN + 3);
                assert!(text.ends_with("..."));
            }
            other => panic!("expected a line error, got {other:?}"),
        }
    }

    #[test]
    fn reports_the_offending_line_in_the_message() {
        let error = validate("CONFIG_MACH_STM32=y\n$(shell id)\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "line 2 of the MCU-reported Kconfig is not a plain Kconfig setting (missing '='): \"$(shell id)\""
        );
    }
}
