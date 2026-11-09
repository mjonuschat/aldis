//! The agent's optional TOML configuration and how it merges with `--moonraker` flags.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The Moonraker used when nothing is configured.
pub const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

/// The contents of a `--config` file. Unknown keys are rejected so typos fail loudly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// Moonraker URLs, one per printer.
    #[serde(default)]
    pub moonraker: Vec<String>,
    /// The `beacon_klipper` checkout Beacon firmware is read from, when it is not the one
    /// Klipper's `beacon.py` is linked from or `~/beacon_klipper`.
    #[serde(default)]
    pub beacon_source: Option<PathBuf>,
    /// The `cartographer_firmware` clone Cartographer firmware is read from, when it is not
    /// `~/cartographer_firmware`.
    #[serde(default)]
    pub cartographer_source: Option<PathBuf>,
    /// A PEM file of the certificates to trust for an `https://` Moonraker, in place of the
    /// public roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Accept any certificate from an `https://` Moonraker without checking it.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

/// Where firmware that is not built from Klipper is read from on this host; `None` looks in
/// each project's default place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirmwareSources {
    pub beacon: Option<PathBuf>,
    pub cartographer: Option<PathBuf>,
}

impl FirmwareSources {
    /// The sources a configuration file names, if any.
    pub fn from_config(file: Option<&FleetConfig>) -> Self {
        Self {
            beacon: file.and_then(|file| file.beacon_source.clone()),
            cartographer: file.and_then(|file| file.cartographer_source.clone()),
        }
    }
}

impl FleetConfig {
    pub fn tls_options(&self) -> crate::tls::TlsOptions {
        crate::tls::TlsOptions {
            ca_file: self.ca_file.clone(),
            insecure_skip_verify: self.insecure_skip_verify,
        }
    }
}

/// Why a configuration file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read {}", path.display())]
    Read {
        /// The file path.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid configuration.
    #[error("invalid configuration in {}: {source}", path.display())]
    Parse {
        /// The file path.
        path: PathBuf,
        /// The parse failure.
        #[source]
        source: toml::de::Error,
    },
}

impl FleetConfig {
    /// Parses configuration text.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads and parses the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// The Moonraker URLs to serve: the file's, then the flags', with a trailing `/` removed, blanks
/// dropped and duplicates collapsed. The default URL when nothing is configured.
pub fn instance_urls(file: Option<FleetConfig>, flags: &[String]) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    let configured = file.into_iter().flat_map(|file| file.moonraker);
    for url in configured.chain(flags.iter().cloned()) {
        let url = url.trim().trim_end_matches('/').to_owned();
        if !url.is_empty() && !urls.contains(&url) {
            urls.push(url);
        }
    }
    if urls.is_empty() {
        urls.push(DEFAULT_MOONRAKER_URL.to_owned());
    }
    urls
}

/// The first URL the agent cannot serve: one that starts with neither `http://` nor `https://`.
pub fn unsupported_url(urls: &[String]) -> Option<&str> {
    urls.iter()
        .map(String::as_str)
        .find(|url| !url.starts_with("http://") && !url.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_first_url_the_agent_cannot_connect_to() {
        assert_eq!(
            unsupported_url(&strings(&["http://127.0.0.1:7125", "https://printer.lan"])),
            None
        );
        assert_eq!(
            unsupported_url(&strings(&[
                "http://127.0.0.1:7125",
                "ws://127.0.0.1:7130",
                "127.0.0.1:7126",
            ])),
            Some("ws://127.0.0.1:7130")
        );
        assert_eq!(
            unsupported_url(&strings(&["127.0.0.1:7126"])),
            Some("127.0.0.1:7126")
        );
    }

    #[test]
    fn reads_the_certificate_verification_settings() {
        let config = FleetConfig::parse(
            "ca_file = \"/etc/ssl/moonraker.pem\"\nmoonraker = [\"https://printer.lan\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.tls_options(),
            crate::tls::TlsOptions {
                ca_file: Some(PathBuf::from("/etc/ssl/moonraker.pem")),
                insecure_skip_verify: false,
            }
        );
        let config = FleetConfig::parse("insecure_skip_verify = true\n").unwrap();
        assert!(config.tls_options().insecure_skip_verify);
        assert_eq!(
            FleetConfig::default().tls_options(),
            crate::tls::TlsOptions::default()
        );
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn defaults_to_the_local_moonraker_when_nothing_is_configured() {
        assert_eq!(instance_urls(None, &[]), strings(&[DEFAULT_MOONRAKER_URL]));
    }

    #[test]
    fn uses_flags_alone_or_the_file_alone() {
        assert_eq!(
            instance_urls(None, &strings(&["http://127.0.0.1:7126"])),
            strings(&["http://127.0.0.1:7126"])
        );
        let file = FleetConfig::parse(r#"moonraker = ["http://127.0.0.1:7125"]"#).unwrap();
        assert_eq!(
            instance_urls(Some(file), &[]),
            strings(&["http://127.0.0.1:7125"])
        );
    }

    #[test]
    fn flags_add_to_the_file_and_duplicates_collapse() {
        let file = FleetConfig::parse(r#"moonraker = ["http://127.0.0.1:7125/"]"#).unwrap();
        assert_eq!(
            instance_urls(
                Some(file),
                &strings(&["http://127.0.0.1:7125", "http://127.0.0.1:7126/", " "]),
            ),
            strings(&["http://127.0.0.1:7125", "http://127.0.0.1:7126"])
        );
    }

    #[test]
    fn reads_where_beacon_and_cartographer_firmware_come_from() {
        let file = FleetConfig::parse(
            r#"
            moonraker = ["http://127.0.0.1:7125"]
            beacon_source = "/opt/beacon_klipper"
            cartographer_source = "/opt/cartographer_firmware"
            "#,
        )
        .unwrap();

        assert_eq!(
            FirmwareSources::from_config(Some(&file)),
            FirmwareSources {
                beacon: Some(PathBuf::from("/opt/beacon_klipper")),
                cartographer: Some(PathBuf::from("/opt/cartographer_firmware")),
            }
        );
        assert_eq!(
            FirmwareSources::from_config(Some(&FleetConfig::default())),
            FirmwareSources::default()
        );
        assert_eq!(
            FirmwareSources::from_config(None),
            FirmwareSources::default()
        );
    }

    #[test]
    fn rejects_unknown_keys_and_reports_unreadable_files() {
        assert!(FleetConfig::parse("moonrakr = []").is_err());
        assert_eq!(FleetConfig::parse("").unwrap(), FleetConfig::default());

        let dir = tempfile::tempdir().unwrap();
        let missing = FleetConfig::load(&dir.path().join("absent.toml")).unwrap_err();
        assert!(matches!(missing, ConfigError::Read { .. }));
        let path = dir.path().join("aldis.toml");
        std::fs::write(&path, "moonraker = 5").unwrap();
        let invalid = FleetConfig::load(&path).unwrap_err();
        assert!(matches!(invalid, ConfigError::Parse { .. }));
        assert!(invalid.to_string().contains("aldis.toml"));
    }
}
