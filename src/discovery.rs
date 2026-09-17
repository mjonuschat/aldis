use std::io::{IsTerminal, Write, stderr};
use std::time::Duration;

use aldis::moonraker::{McuInventory, MoonrakerError, MoonrakerPort};
use aldis::retry::retry_until_available;

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RETRIES: u32 = 3;

pub(crate) fn discover_mcus_with_retry(
    client: &impl MoonrakerPort,
) -> Result<McuInventory, MoonrakerError> {
    discover_mcus_with_retry_params(client, TIMEOUT, POLL_INTERVAL, MAX_RETRIES)
}

fn discover_mcus_with_retry_params(
    client: &impl MoonrakerPort,
    timeout: Duration,
    poll_interval: Duration,
    max_retries: u32,
) -> Result<McuInventory, MoonrakerError> {
    let interactive = stderr().is_terminal();
    let mut retries = 0u32;
    let outcome = retry_until_available(timeout, poll_interval, || match client.discover_mcus() {
        Ok(inventory) => Ok(Ok(inventory)),
        Err(error) if is_klipper_not_ready(&error) => {
            retries += 1;
            print_waiting(interactive, retries, max_retries);
            Err(error)
        }
        Err(error) => Ok(Err(error)),
    });
    clear_waiting_line(interactive, retries > 0);
    outcome.unwrap_or_else(Err)
}

fn is_klipper_not_ready(error: &MoonrakerError) -> bool {
    matches!(
        error,
        MoonrakerError::KlippyStarting(_) | MoonrakerError::KlippyNotConnected
    )
}

fn print_waiting(interactive: bool, attempt: u32, max: u32) {
    if interactive {
        eprint!("\r\x1b[2KWaiting for Klipper to become ready... ({attempt}/{max})");
    } else {
        eprintln!("Waiting for Klipper to become ready... ({attempt}/{max})");
    }
    let _ = stderr().flush();
}

fn clear_waiting_line(interactive: bool, waited: bool) {
    if interactive && waited {
        eprint!("\r\x1b[2K");
        let _ = stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use aldis::moonraker::{McuInventory, MoonrakerError};

    use super::discover_mcus_with_retry_params;

    struct FakeMoonraker {
        responses: RefCell<Vec<Result<McuInventory, MoonrakerError>>>,
        calls: RefCell<u32>,
    }

    impl aldis::moonraker::MoonrakerPort for FakeMoonraker {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            *self.calls.borrow_mut() += 1;
            self.responses.borrow_mut().remove(0)
        }
    }

    #[test]
    fn stops_immediately_on_a_non_retryable_error() {
        let client = FakeMoonraker {
            responses: RefCell::new(vec![Err(MoonrakerError::InvalidResponse(
                "no MCU objects were reported".to_owned(),
            ))]),
            calls: RefCell::new(0),
        };

        let result = discover_mcus_with_retry_params(
            &client,
            Duration::from_millis(30),
            Duration::from_millis(10),
            3,
        );

        assert!(matches!(result, Err(MoonrakerError::InvalidResponse(_))));
        assert_eq!(*client.calls.borrow(), 1);
    }

    #[test]
    fn retries_while_klipper_is_starting_and_succeeds_once_ready() {
        let client = FakeMoonraker {
            responses: RefCell::new(vec![
                Err(MoonrakerError::KlippyStarting("starting".to_owned())),
                Err(MoonrakerError::KlippyNotConnected),
                Ok(McuInventory { mcus: Vec::new() }),
            ]),
            calls: RefCell::new(0),
        };

        let result = discover_mcus_with_retry_params(
            &client,
            Duration::from_millis(60),
            Duration::from_millis(10),
            3,
        );

        assert!(result.is_ok());
        assert_eq!(*client.calls.borrow(), 3);
    }

    struct NeverReady;

    impl aldis::moonraker::MoonrakerPort for NeverReady {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            Err(MoonrakerError::KlippyStarting("still starting".to_owned()))
        }
    }

    #[test]
    fn returns_the_last_not_ready_error_after_the_timeout() {
        let result = discover_mcus_with_retry_params(
            &NeverReady,
            Duration::from_millis(30),
            Duration::from_millis(10),
            3,
        );

        assert!(matches!(result, Err(MoonrakerError::KlippyStarting(_))));
    }
}
