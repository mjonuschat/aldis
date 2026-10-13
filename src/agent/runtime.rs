//! Runs one agent connection per configured Moonraker inside a single process.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::agent::connection::run_forever;
use crate::agent::service::{AgentService, LockHolders};
use crate::agent::system::SystemBackend;
use crate::logging::RunLogSink;
use crate::moonraker::{HostPort, MoonrakerAdapter};
use crate::service::DEFAULT_KLIPPER_UNIT;

/// A configured Moonraker and the Klipper unit it manages, when that could be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The Moonraker URL.
    pub url: String,
    /// The Klipper unit; `None` when Moonraker could not be asked (it may still be starting).
    pub unit: Option<String>,
}

/// Splits `found` into the instances to serve and those that repeat a Klipper unit already seen
/// (the first URL per unit wins). Instances with an unknown unit are never merged.
pub fn dedupe_by_unit(found: Vec<Found>) -> (Vec<Found>, Vec<Found>) {
    let mut seen = HashSet::new();
    let mut refused = Vec::new();
    let kept = found
        .into_iter()
        .filter_map(|instance| match &instance.unit {
            Some(unit) if !seen.insert(unit.clone()) => {
                refused.push(instance);
                None
            }
            _ => Some(instance),
        })
        .collect();
    (kept, refused)
}

/// Serves every Moonraker in `urls`, each on its own connection, and never returns.
pub fn run_instances(urls: Vec<String>, sink: RunLogSink) -> ! {
    let found = urls
        .into_iter()
        .map(|url| {
            let unit = match MoonrakerAdapter::new(&url).klipper_unit() {
                Ok(unit) => Some(unit.unwrap_or_else(|| DEFAULT_KLIPPER_UNIT.to_owned())),
                Err(error) => {
                    tracing::warn!(%url, error = %crate::error_chain(&error), "could not ask Moonraker which Klipper service it manages");
                    None
                }
            };
            Found { url, unit }
        })
        .collect();
    let (kept, refused) = dedupe_by_unit(found);
    for instance in refused {
        tracing::warn!(
            url = %instance.url,
            "ignoring {}: it resolves to the same Klipper service as another configured Moonraker",
            instance.url
        );
    }
    let holders = LockHolders::default();
    for instance in kept {
        let name = instance
            .unit
            .clone()
            .unwrap_or_else(|| instance.url.clone());
        let events = Arc::new(Mutex::new(None));
        let service = AgentService::new(
            SystemBackend::new(instance.url.clone(), sink.clone()),
            Arc::clone(&events),
        )
        .with_identity(name, holders.clone());
        std::thread::spawn(move || run_forever(&instance.url, service, events));
    }
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(url: &str, unit: Option<&str>) -> Found {
        Found {
            url: url.to_owned(),
            unit: unit.map(str::to_owned),
        }
    }

    #[test]
    fn keeps_the_first_url_per_klipper_unit() {
        let (kept, refused) = dedupe_by_unit(vec![
            found("http://127.0.0.1:7125", Some("klipper")),
            found("http://localhost:7125", Some("klipper")),
            found("http://127.0.0.1:7126", Some("klipper-2")),
        ]);
        let urls = |list: &[Found]| list.iter().map(|f| f.url.clone()).collect::<Vec<_>>();
        assert_eq!(
            urls(&kept),
            ["http://127.0.0.1:7125", "http://127.0.0.1:7126"]
        );
        assert_eq!(urls(&refused), ["http://localhost:7125"]);
    }

    #[test]
    fn never_merges_instances_whose_unit_is_unknown() {
        let (kept, refused) = dedupe_by_unit(vec![
            found("http://127.0.0.1:7125", None),
            found("http://127.0.0.1:7126", None),
        ]);
        assert_eq!(kept.len(), 2);
        assert!(refused.is_empty());
    }
}
