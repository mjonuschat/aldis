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
}

/// A ready Katapult bootloader session that can flash one firmware artifact.
///
/// This backend does not enter a bootloader or assign a CAN node. Callers must
/// perform those transport-specific state changes before constructing it.
pub struct KatapultAdapter<T> {
    session: KatapultSession<T>,
    expected_canbus_uuid: Option<u64>,
}

impl<T> KatapultAdapter<T> {
    /// Creates a backend for a serial or otherwise already-identified target.
    pub fn new(transport: T) -> Self {
        Self {
            session: KatapultSession::new(transport),
            expected_canbus_uuid: None,
        }
    }

    /// Creates a backend that verifies the configured CAN UUID before writing.
    pub fn for_canbus(transport: T, expected_canbus_uuid: u64) -> Self {
        Self {
            session: KatapultSession::new(transport),
            expected_canbus_uuid: Some(expected_canbus_uuid),
        }
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

    /// Connects, optionally validates the CAN identity, transfers and verifies
    /// firmware, then starts the application only after successful verification.
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
