//! Native firmware flashing interfaces and transport implementations.

pub mod katapult;
pub mod stm32_dfu;

/// Result details shared by native flashing backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashResult {
    /// The number of firmware pages accepted by the bootloader.
    pub pages_written: u32,
    /// Number of bytes transferred after bootloader-required padding.
    pub padded_bytes: usize,
}

/// A native backend capable of writing and verifying a firmware image.
pub trait FlashBackend {
    /// Backend-specific failure type.
    type Error;

    /// Writes `firmware` and verifies the result through the backend protocol.
    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error>;
}
