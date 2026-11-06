//! A validated request to flash prebuilt firmware: the bytes, how they may be sent, and the one
//! MCU they were prepared for.
//!
//! Prebuilt firmware (FOCI) is not built from the Klipper checkout, so nothing but the vendor's
//! own checks stands between a file and a bootloader. A request can be made only by those checks
//! (see [`crate::prebuilt`]), and it carries what the flash must respect: the address Katapult
//! must start applications at. The coordinator refuses a request that was prepared for a
//! different MCU than the one approved, before Klipper is stopped.

use crate::moonraker::{Mcu, McuTransport};
use crate::prepare::PreparedBuild;

/// The vendors whose firmware aldis flashes without building it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vendor {
    Foci,
}

/// The MCU a request was prepared for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Destination {
    name: String,
    model: String,
    transport: Option<McuTransport>,
}

impl Destination {
    fn of(mcu: &Mcu) -> Self {
        Self {
            name: mcu.name.clone(),
            model: mcu.mcu.clone(),
            transport: mcu.transport.clone(),
        }
    }

    /// The MCU object's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether `prepared` is the MCU this request was made for.
    pub(crate) fn is(&self, prepared: &PreparedBuild) -> bool {
        self.name == prepared.target_name
            && self.model == prepared.mcu
            && self.transport == prepared.transport
    }
}

/// How the bytes of a request are sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Protocol {
    /// To the MCU's Katapult, which must start applications at `application_start`.
    Katapult {
        image: Vec<u8>,
        application_start: u32,
    },
}

/// Prebuilt firmware to flash to one MCU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlashRequest {
    vendor: Vendor,
    destination: Destination,
    protocol: Protocol,
}

impl FlashRequest {
    /// Firmware for `mcu` sent to its Katapult.
    pub(crate) fn katapult(
        vendor: Vendor,
        mcu: &Mcu,
        image: Vec<u8>,
        application_start: u32,
    ) -> Self {
        Self {
            vendor,
            destination: Destination::of(mcu),
            protocol: Protocol::Katapult {
                image,
                application_start,
            },
        }
    }

    pub fn vendor(&self) -> Vendor {
        self.vendor
    }

    pub fn destination(&self) -> &Destination {
        &self.destination
    }

    pub fn protocol(&self) -> &Protocol {
        &self.protocol
    }
}
