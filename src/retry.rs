//! Poll-until-ready retry helper shared by USB, CAN, and Moonraker readiness waits.

use std::thread;
use std::time::{Duration, Instant};

/// Repeatedly calls `operation` until it succeeds or `timeout` elapses.
///
/// Sleeps `poll_interval` (floored to 10ms) between attempts. Returns the
/// last error once the deadline passes.
pub fn retry_until_available<T, E>(
    timeout: Duration,
    poll_interval: Duration,
    mut operation: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let deadline = Instant::now() + timeout;
    loop {
        match operation() {
            Ok(result) => return Ok(result),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(poll_interval.max(Duration::from_millis(10))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::retry_until_available;
    use std::time::Duration;

    #[test]
    fn retries_resource_access_until_it_succeeds() {
        let mut attempts = 0;
        let result = retry_until_available(Duration::from_millis(50), Duration::ZERO, || {
            attempts += 1;
            (attempts == 3).then_some("ready").ok_or("not ready")
        });

        assert_eq!(result, Ok("ready"));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn returns_the_last_error_once_the_deadline_passes() {
        let result =
            retry_until_available(Duration::from_millis(20), Duration::from_millis(5), || {
                Err::<(), _>("still not ready")
            });

        assert_eq!(result, Err("still not ready"));
    }
}
