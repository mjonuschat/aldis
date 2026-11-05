//! Katapult flashing backend assembled from a ready transport session.

use std::fmt;

use super::session::{KatapultSession, SessionError, Transport};
use crate::flash::{FlashPort, FlashResult};

/// A failure while flashing a Katapult target.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KatapultFlashError {
    /// Katapult rejected or could not verify a protocol operation.
    #[error(transparent)]
    Session(SessionError),
    /// Katapult starts its application somewhere other than where the firmware is linked to
    /// run, so the written image would not boot. Nothing was written.
    #[error(
        "Katapult starts applications at {actual:#x}, but the firmware is linked to start at {expected:#x}"
    )]
    ApplicationStartMismatch { expected: u32, actual: u32 },
}

/// A ready Katapult bootloader session that can flash one firmware artifact.
///
/// This backend does not enter a bootloader or assign a CAN node. Callers must
/// perform those transport-specific state changes before constructing it.
pub struct KatapultAdapter<T> {
    session: KatapultSession<T>,
    expected_canbus_uuid: Option<u64>,
    expected_application_start: Option<u32>,
}

impl<T> KatapultAdapter<T> {
    /// Creates a backend for a serial or otherwise already-identified target.
    pub fn new(transport: T) -> Self {
        Self {
            session: KatapultSession::new(transport),
            expected_canbus_uuid: None,
            expected_application_start: None,
        }
    }

    /// Creates a backend that verifies the configured CAN UUID before writing.
    pub fn for_canbus(transport: T, expected_canbus_uuid: u64) -> Self {
        Self {
            session: KatapultSession::new(transport),
            expected_canbus_uuid: Some(expected_canbus_uuid),
            expected_application_start: None,
        }
    }

    /// Refuses, before writing, a Katapult that starts its application anywhere but
    /// `application_start`, the address the firmware is linked to run from. Katapult builds
    /// carry no Kconfig to check this against, so prebuilt firmware relies on it.
    pub fn expecting_application_start(mut self, application_start: u32) -> Self {
        self.expected_application_start = Some(application_start);
        self
    }

    /// Returns the underlying transport after the flashing session ends.
    pub fn into_transport(self) -> T {
        self.session.into_transport()
    }
}

impl<T: Transport> FlashPort for KatapultAdapter<T>
where
    T::Error: fmt::Debug,
{
    type Error = KatapultFlashError;

    /// Connects, optionally validates the CAN identity and application start, transfers and
    /// verifies firmware, then starts the application only after successful verification.
    /// A mismatched application start leaves the existing application in place and starts it.
    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error> {
        let connection = self
            .session
            .connect()
            .map_err(KatapultFlashError::Session)?;
        if let Some(expected_canbus_uuid) = self.expected_canbus_uuid {
            self.session
                .verify_canbus_uuid(expected_canbus_uuid)
                .map_err(KatapultFlashError::Session)?;
        }
        let actual = connection.target.application_start();
        if let Some(expected) = self.expected_application_start
            && actual != expected
        {
            if let Err(error) = self.session.complete() {
                tracing::debug!(
                    ?error,
                    "could not start the existing application after refusing the flash"
                );
            }
            return Err(KatapultFlashError::ApplicationStartMismatch { expected, actual });
        }
        let upload = self
            .session
            .upload(firmware, connection.target)
            .map_err(KatapultFlashError::Session)?;
        self.session
            .complete()
            .map_err(KatapultFlashError::Session)?;
        Ok(FlashResult {
            reported_pages: Some(upload.pages_written),
            padded_bytes: upload.padded_bytes,
        })
    }
}
