//! Checks that the Moonraker aldis talks to is local and fronts a Klipper service whose name
//! aldis's service control and sudoers policy allow.

use crate::moonraker::HostPort;
use crate::service::KlipperUnit;

/// Why aldis refuses to touch hardware for a Moonraker instance.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostCheckError {
    /// The Moonraker URL does not point at this machine.
    #[error("Moonraker at {0} is not on this machine; firmware changes require a local Moonraker")]
    NonLocal(String),
    /// Moonraker manages a service whose name is not a valid Klipper service name.
    #[error(
        "Moonraker manages the Klipper service {0:?}, which is not a valid Klipper service name"
    )]
    UnsupportedInstance(String),
    /// Moonraker could not be asked which Klipper service it manages.
    #[error("could not ask Moonraker which Klipper service it manages: {0}")]
    Unavailable(String),
}

/// Whether `url`'s host is `localhost` or a loopback IP address.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Checks the URL and Moonraker's reported unit and returns the unit to control; an unreported unit
/// is assumed to be the default.
pub fn check_host(url: &str, klipper_unit: Option<&str>) -> Result<KlipperUnit, HostCheckError> {
    if !is_loopback_url(url) {
        return Err(HostCheckError::NonLocal(url.to_owned()));
    }
    match klipper_unit {
        Some(unit) => KlipperUnit::parse(unit)
            .map_err(|_| HostCheckError::UnsupportedInstance(unit.to_owned())),
        None => Ok(KlipperUnit::default()),
    }
}

/// [`check_host`] with the unit fetched from Moonraker. The URL is checked first so a remote
/// Moonraker is never queried. A response without the field counts as the default unit; a failed
/// query does not.
pub fn verify_host(url: &str, moonraker: &impl HostPort) -> Result<KlipperUnit, HostCheckError> {
    check_host(url, None)?;
    let unit = moonraker
        .klipper_unit()
        .map_err(|error| HostCheckError::Unavailable(crate::error_chain(&error)))?;
    check_host(url, unit.as_deref())
}
