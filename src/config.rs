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

#[cfg(test)]
mod tests {
    use super::*;

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
