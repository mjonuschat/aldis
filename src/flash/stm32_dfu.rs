//! STM32 USB-DFU backend boundaries backed by `dfu-rs`.

/// An explicitly selected STM32 DFU USB identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stm32DfuDevice {
    /// USB vendor identifier.
    pub vendor_id: u16,
    /// USB product identifier.
    pub product_id: u16,
}

impl Stm32DfuDevice {
    /// STMicroelectronics' built-in ROM DFU bootloader identity.
    pub const ROM_BOOTLOADER: Self = Self {
        vendor_id: 0x0483,
        product_id: 0xdf11,
    };
}

/// STM32 flash placement required by the DFU backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stm32DfuTarget {
    /// Explicit application start address from the embedded build configuration.
    pub application_start: u32,
}
