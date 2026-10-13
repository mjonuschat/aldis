//! Reads another printer's live state from its Moonraker.

use std::time::Duration;

use crate::fleet::peer::PeerState;
use crate::moonraker::{
    HostPort, KlippyState, McuInventory, MoonrakerAdapter, MoonrakerError, PrinterStatePort,
};
use crate::service::DEFAULT_KLIPPER_UNIT;

const PEER_TIMEOUT: Duration = Duration::from_secs(2);

/// Reads `url`'s Moonraker now. Anything that cannot be established is `None`, never assumed idle.
/// A Klipper that failed to load its config owns no MCUs, so it reads as an empty inventory.
pub fn probe_peer(url: &str) -> PeerState {
    let adapter = MoonrakerAdapter::with_timeout(url, PEER_TIMEOUT);
    let unit = match adapter.klipper_unit() {
        Ok(Some(unit)) => Some(unit),
        Ok(None) => Some(DEFAULT_KLIPPER_UNIT.to_owned()),
        Err(_) => None,
    };
    let klippy = adapter.host_info().ok().map(|info| info.state);
    let print_state = match klippy {
        Some(KlippyState::Ready) => adapter.print_state().ok(),
        _ => None,
    };
    let inventory = match adapter.discover_mcus() {
        Ok(inventory) => Some(inventory),
        Err(MoonrakerError::ConfigError(_)) => Some(McuInventory {
            mcus: Vec::new(),
            unreported: Vec::new(),
        }),
        Err(_) => None,
    };
    PeerState {
        name: unit.clone().unwrap_or_else(|| url.to_owned()),
        url: url.to_owned(),
        unit,
        klippy,
        print_state,
        inventory,
    }
}

/// Every URL in `all` except `own`, comparing with a trailing `/` trimmed.
pub fn other_urls(own: &str, all: &[String]) -> Vec<String> {
    let own = own.trim_end_matches('/');
    all.iter()
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| *url != own)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_url_but_our_own() {
        let all = [
            "http://127.0.0.1:7125".to_owned(),
            "http://127.0.0.1:7126".to_owned(),
            "http://127.0.0.1:7127".to_owned(),
        ];
        assert_eq!(
            other_urls("http://127.0.0.1:7126/", &all),
            ["http://127.0.0.1:7125", "http://127.0.0.1:7127"]
        );
        assert_eq!(
            other_urls("http://127.0.0.1:7125", &all[..1]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_unreachable_moonraker_is_reported_as_unknown_not_idle() {
        // Port 1 on loopback refuses connections immediately.
        let peer = probe_peer("http://127.0.0.1:1");
        assert_eq!(peer.name, "http://127.0.0.1:1");
        assert_eq!(peer.unit, None);
        assert_eq!(peer.klippy, None);
        assert_eq!(peer.print_state, None);
        assert_eq!(peer.inventory, None);
    }
}
