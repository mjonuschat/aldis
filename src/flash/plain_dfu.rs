//! A plain DFU 1.x download, as `dfu-util -D` does it: no DfuSe memory map and no address, the
//! bootloader decides where the image goes. Beacon's and INDX's bootloaders both speak it.

use std::path::Path;
use std::time::Duration;

use dfu_core::DfuIo;
use dfu_core::DfuProtocol;
use dfu_core::functional_descriptor::FunctionalDescriptor;
use dfu_core::memory_layout::MemoryLayout;
use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};

/// DFU's USB interface class and subclass.
const DFU_CLASS: u8 = 0xfe;
const DFU_SUBCLASS: u8 = 0x01;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

/// A DFU device's control transfers over `nusb`.
///
/// `dfu_nusb::DfuNusb` is the same, except that it reads the interface's string descriptor on
/// opening, which only a DfuSe memory layout needs. The INDX bootloader answers the language-list
/// request with far more bytes than the descriptor holds, which `nusb` rejects, so a plain
/// download must not ask.
struct PlainDfuIo {
    device: nusb::Device,
    interface: nusb::Interface,
    descriptor: FunctionalDescriptor,
    protocol: DfuProtocol<MemoryLayout>,
}

impl PlainDfuIo {
    fn open(device: nusb::Device, interface: nusb::Interface) -> Result<Self, dfu_nusb::Error> {
        let descriptor = interface
            .descriptors()
            .find_map(|alt| {
                alt.descriptors()
                    .find_map(|descriptor| FunctionalDescriptor::from_bytes(&descriptor))
            })
            .ok_or(dfu_nusb::Error::FunctionalDescriptorNotFound)??;
        let protocol = DfuProtocol::new("", descriptor.dfu_version)?;
        Ok(Self {
            device,
            interface,
            descriptor,
            protocol,
        })
    }
}

fn split_request_type(request_type: u8) -> (ControlType, Recipient) {
    (
        match (request_type >> 5) & 0x03 {
            0 => ControlType::Standard,
            1 => ControlType::Class,
            _ => ControlType::Vendor,
        },
        match request_type & 0x1f {
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            3 => Recipient::Other,
            _ => Recipient::Device,
        },
    )
}

impl DfuIo for PlainDfuIo {
    type Read = usize;
    type Write = usize;
    type Reset = ();
    type Error = dfu_nusb::Error;
    type MemoryLayout = MemoryLayout;

    fn read_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        buffer: &mut [u8],
    ) -> Result<Self::Read, Self::Error> {
        let (control_type, recipient) = split_request_type(request_type);
        let reply = self
            .interface
            .control_in(
                ControlIn {
                    control_type,
                    recipient,
                    request,
                    value,
                    index: u16::from(self.interface.interface_number()),
                    length: buffer.len().try_into().unwrap_or(u16::MAX),
                },
                CONTROL_TIMEOUT,
            )
            .wait()?;
        let length = reply.len().min(buffer.len());
        buffer[..length].copy_from_slice(&reply[..length]);
        Ok(length)
    }

    fn write_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        buffer: &[u8],
    ) -> Result<Self::Write, Self::Error> {
        let (control_type, recipient) = split_request_type(request_type);
        self.interface
            .control_out(
                ControlOut {
                    control_type,
                    recipient,
                    request,
                    value,
                    index: u16::from(self.interface.interface_number()),
                    data: buffer,
                },
                CONTROL_TIMEOUT,
            )
            .wait()?;
        Ok(buffer.len())
    }

    fn usb_reset(self) -> Result<Self::Reset, Self::Error> {
        // The interface is released first: a claimed interface blocks a reset on some hosts.
        drop(self.interface);
        self.device.reset().wait()?;
        Ok(())
    }

    fn protocol(&self) -> &DfuProtocol<Self::MemoryLayout> {
        &self.protocol
    }

    fn functional_descriptor(&self) -> &FunctionalDescriptor {
        &self.descriptor
    }
}

/// Failure during a plain DFU download.
#[derive(Debug, thiserror::Error)]
pub enum PlainDfuError {
    /// USB enumeration, opening the device or claiming its interface failed.
    #[error("{0}")]
    Discovery(#[source] nusb::Error),
    /// No device with that identity was at the topology, or it exposed no DFU interface.
    #[error("no DFU device was found at its USB topology")]
    DeviceNotFound,
    /// The DFU transfer failed.
    #[error("{0}")]
    Transport(#[source] dfu_nusb::Error),
}

/// Downloads `payload` to the DFU device `vendor_id:product_id` at `sysfs_path` and starts its
/// manifestation. A bootloader that resets into the new application as soon as it is asked to
/// manifest leaves the bus mid-poll. That is not a failure, because the whole image has been
/// downloaded by then. Whether the new firmware runs is for the caller to verify.
pub fn download(
    sysfs_path: &Path,
    vendor_id: u16,
    product_id: u16,
    payload: &[u8],
) -> Result<(), PlainDfuError> {
    crate::flash::block_on(async {
        let mut matches = nusb::list_devices()
            .await
            .map_err(PlainDfuError::Discovery)?
            .filter(|device| {
                device.sysfs_path() == sysfs_path
                    && device.vendor_id() == vendor_id
                    && device.product_id() == product_id
            });
        let device_info = matches.next().ok_or(PlainDfuError::DeviceNotFound)?;
        let interface_number = device_info
            .interfaces()
            .find(|interface| {
                interface.class() == DFU_CLASS && interface.subclass() == DFU_SUBCLASS
            })
            .map(|interface| interface.interface_number())
            .ok_or(PlainDfuError::DeviceNotFound)?;
        let device = device_info.open().await.map_err(PlainDfuError::Discovery)?;
        let interface = device
            .detach_and_claim_interface(interface_number)
            .await
            .map_err(PlainDfuError::Discovery)?;
        interface
            .set_alt_setting(0)
            .await
            .map_err(|error| PlainDfuError::Transport(error.into()))?;
        let dfu = dfu_core::synchronous::DfuSync::new(
            PlainDfuIo::open(device, interface).map_err(PlainDfuError::Transport)?,
        )
        .download_without_manifest_from_slice(payload)
        .map_err(PlainDfuError::Transport)?;
        match dfu.manifest() {
            Ok(_) => Ok(()),
            Err(error) if manifestation_finished_early(&error) => {
                tracing::debug!(%error, "dfu bootloader finished manifestation before it was polled");
                Ok(())
            }
            Err(error) => Err(PlainDfuError::Transport(error)),
        }
    })
}

/// Whether an error from asking for the manifestation only means the bootloader acted on it
/// before the host could watch: it dropped off the bus to restart into the new firmware, or it
/// answered the status poll from `DfuIdle`, having already finished (it never reports
/// `DfuManifest`). The image has been downloaded in full by then.
fn manifestation_finished_early(error: &dfu_nusb::Error) -> bool {
    use dfu_core::State;
    match error {
        dfu_nusb::Error::Transfer(_) | dfu_nusb::Error::Nusb(_) => true,
        dfu_nusb::Error::Dfu(dfu_core::Error::InvalidState { got, expected }) => {
            *got == State::DfuIdle
                && matches!(expected, State::DfuManifest | State::DfuManifestSync)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::manifestation_finished_early;

    #[test]
    fn counts_a_bootloader_that_is_already_idle_after_the_last_block_as_finished() {
        use dfu_core::State;

        let invalid =
            |got, expected| dfu_nusb::Error::Dfu(dfu_core::Error::InvalidState { got, expected });

        assert!(manifestation_finished_early(&invalid(
            State::DfuIdle,
            State::DfuManifest
        )));
        assert!(manifestation_finished_early(&invalid(
            State::DfuIdle,
            State::DfuManifestSync
        )));
        assert!(
            !manifestation_finished_early(&invalid(State::DfuError, State::DfuManifest)),
            "a device in error did not finish"
        );
        assert!(
            !manifestation_finished_early(&invalid(State::DfuIdle, State::DfuDnbusy)),
            "only the manifestation may end this early"
        );
        assert!(!manifestation_finished_early(
            &dfu_nusb::Error::DeviceNotFound
        ));
    }
}
