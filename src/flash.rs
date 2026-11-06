//! Native firmware flashing interfaces and transport implementations.

pub mod katapult;
pub mod linux_host;
pub mod picoboot;
pub mod reboot;
pub mod request;
pub mod samba;
pub mod stm32_dfu;
pub mod system;
pub mod usb_bootloader;
pub(crate) mod usb_sysfs;

/// Runs `future` to completion on a fresh single-threaded Tokio runtime: the USB backends are
/// async libraries, but every flash in aldis is one synchronous operation.
pub(crate) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(future)
}

/// Result details shared by native flashing backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashResult {
    /// The number of firmware pages reported by the bootloader, when available.
    pub reported_pages: Option<u32>,
    /// Number of bytes transferred after bootloader-required padding.
    pub padded_bytes: usize,
}

/// A port for writing and verifying a firmware image, implemented by each native adapter.
pub trait FlashPort {
    /// Adapter-specific failure type.
    type Error;

    /// Writes `firmware` and verifies the result through the adapter's protocol.
    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error>;
}
