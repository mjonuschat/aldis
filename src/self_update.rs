//! Discovering, downloading, and installing a released `aldis` binary.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::build::{BuildCommand, CommandError, CommandPort};

/// Errors while checking for or installing a released `aldis` binary.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The GitHub API request failed.
    #[error("GitHub request failed")]
    Http(#[from] ureq::Error),
    /// GitHub's response body could not be parsed.
    #[error("GitHub returned invalid JSON")]
    Json(#[from] serde_json::Error),
    /// The host's CPU architecture has no published release archive.
    #[error("no published release archive for host architecture {0:?}")]
    UnsupportedPlatform(String),
    /// The downloaded archive could not be extracted.
    #[error("could not extract the downloaded archive")]
    Extract(#[source] CommandError),
    /// `tar` ran but reported failure.
    #[error("tar failed: {0}")]
    ExtractFailed(String),
    /// The extracted archive did not contain an `aldis` binary.
    #[error("downloaded archive did not contain an aldis binary")]
    MissingBinary,
    /// Installing the new binary over the running one failed.
    #[error("could not install the new binary")]
    Install(#[source] io::Error),
}

/// The latest published `aldis` release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// The release tag, e.g. `"v0.2.3"`.
    pub tag: String,
}

/// A source of the latest published `aldis` release and its archives.
///
/// Isolates callers from the concrete `ureq`-backed [`GithubReleaseAdapter`]
/// so the update-available decision can be tested against a fake.
pub trait ReleasePort {
    /// Queries GitHub for the latest published release.
    fn latest_release(&self) -> Result<LatestRelease, ReleaseError>;
    /// Downloads the release archive for `platform` (e.g. `"aarch64-linux"`).
    fn download_archive(&self, platform: &str) -> Result<Vec<u8>, ReleaseError>;
}

/// Fetches releases from a GitHub repository over HTTPS.
pub struct GithubReleaseAdapter {
    agent: ureq::Agent,
    repo: String,
}

impl GithubReleaseAdapter {
    /// Creates an adapter for `repo` (e.g. `"mjonuschat/aldis"`).
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
            repo: repo.into(),
        }
    }
}

impl ReleasePort for GithubReleaseAdapter {
    fn latest_release(&self) -> Result<LatestRelease, ReleaseError> {
        #[derive(serde::Deserialize)]
        struct Response {
            tag_name: String,
        }

        let response = self
            .agent
            .get(format!(
                "https://api.github.com/repos/{}/releases/latest",
                self.repo
            ))
            .header("User-Agent", "aldis-self-update")
            .header("Accept", "application/vnd.github+json")
            .call()?
            .body_mut()
            .read_to_string()?;
        let parsed: Response = serde_json::from_str(&response)?;
        Ok(LatestRelease {
            tag: parsed.tag_name,
        })
    }

    fn download_archive(&self, platform: &str) -> Result<Vec<u8>, ReleaseError> {
        let mut bytes = Vec::new();
        self.agent
            .get(download_url(&self.repo, platform))
            .header("User-Agent", "aldis-self-update")
            .call()?
            .body_mut()
            .as_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| ReleaseError::Http(ureq::Error::from(error)))?;
        Ok(bytes)
    }
}

/// Returns whether `current_version` (e.g. `"0.2.3"`, matching
/// `CARGO_PKG_VERSION`) already matches `latest_tag` (e.g. `"v0.2.3"`).
pub fn is_up_to_date(current_version: &str, latest_tag: &str) -> bool {
    latest_tag.trim_start_matches('v') == current_version
}

/// Maps the host architecture to the platform suffix release archives are
/// published under (`aldis-<platform>.tar.xz`), matching the targets
/// `release.yml` builds.
pub fn target_platform(arch: &str) -> Result<&'static str, ReleaseError> {
    match arch {
        "x86_64" => Ok("x86_64-linux"),
        "aarch64" => Ok("aarch64-linux"),
        other => Err(ReleaseError::UnsupportedPlatform(other.to_owned())),
    }
}

fn download_url(repo: &str, platform: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/aldis-{platform}.tar.xz")
}

/// Extracts `archive` (a `.tar.xz`, as published) into `dest_dir`, stripping
/// the archive's own wrapper directory, and returns the extracted binary's path.
pub fn extract_binary(
    command_runner: &impl CommandPort,
    archive: &Path,
    dest_dir: &Path,
) -> Result<PathBuf, ReleaseError> {
    let output = command_runner
        .run(&BuildCommand {
            program: "tar".to_owned(),
            arguments: vec![
                "xJf".to_owned(),
                archive.display().to_string(),
                "-C".to_owned(),
                dest_dir.display().to_string(),
                "--strip-components=1".to_owned(),
            ],
            current_dir: None,
        })
        .map_err(ReleaseError::Extract)?;
    if !output.success {
        return Err(ReleaseError::ExtractFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let binary = dest_dir.join("aldis");
    if !binary.exists() {
        return Err(ReleaseError::MissingBinary);
    }
    Ok(binary)
}

/// Atomically replaces `current_exe` with `new_binary`: stages a copy
/// alongside it (same directory, so the final rename is atomic and stays on
/// one filesystem), marks it executable, then renames it into place. Safe
/// even while `current_exe` is the file backing the running process — Linux
/// keeps its old inode mapped until the process exits.
pub fn install_binary(new_binary: &Path, current_exe: &Path) -> Result<(), ReleaseError> {
    install_binary_impl(new_binary, current_exe).map_err(ReleaseError::Install)
}

fn install_binary_impl(new_binary: &Path, current_exe: &Path) -> io::Result<()> {
    let parent = current_exe
        .parent()
        .ok_or_else(|| io::Error::other("current executable has no parent directory"))?;
    let staged = parent.join(".aldis.new");
    fs::copy(new_binary, &staged)?;
    let mut permissions = fs::metadata(&staged)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(&staged, permissions)?;
    fs::rename(&staged, current_exe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_up_to_date, target_platform};

    #[test]
    fn matches_a_tag_with_or_without_its_v_prefix() {
        assert!(is_up_to_date("0.2.3", "v0.2.3"));
        assert!(!is_up_to_date("0.2.2", "v0.2.3"));
    }

    #[test]
    fn maps_supported_host_architectures_to_release_platform_names() {
        assert_eq!(target_platform("x86_64").unwrap(), "x86_64-linux");
        assert_eq!(target_platform("aarch64").unwrap(), "aarch64-linux");
        assert!(target_platform("riscv64").is_err());
    }
}
