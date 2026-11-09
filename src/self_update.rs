//! Discovering, downloading, and installing a released `aldis` binary.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::build::{BuildCommand, CommandError, CommandPort};

/// The checksum manifest published with every release, as written by `sha256sum`.
pub const CHECKSUMS_FILE: &str = "SHA256SUMS";

/// The maximum encoded and decoded archive size. Published archives are a few MiB; one extra
/// byte may be read to detect an oversized response without buffering past this cap.
pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// Errors while checking for or installing a released `aldis` binary.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The GitHub API request failed.
    #[error("GitHub request failed: {0}")]
    Http(#[from] ureq::Error),
    /// The archive response kept going past [`MAX_ARCHIVE_BYTES`].
    #[error("the release archive is larger than the {0} bytes aldis will download")]
    ArchiveTooLarge(u64),
    /// GitHub's response body could not be parsed.
    #[error("GitHub returned invalid JSON")]
    Json(#[from] serde_json::Error),
    /// The host's CPU architecture has no published release archive.
    #[error("no published release archive for host architecture {0:?}")]
    UnsupportedPlatform(String),
    /// The release's checksum manifest does not list the archive.
    #[error("the release's {CHECKSUMS_FILE} has no entry for {0}")]
    ChecksumMissing(String),
    /// The manifest lists the archive, but not with a SHA-256 digest.
    #[error("the release's {CHECKSUMS_FILE} entry for {0} is not a SHA-256 digest")]
    ChecksumInvalid(String),
    /// The downloaded archive's digest differs from the published one.
    #[error(
        "downloaded {file} does not match the release's {CHECKSUMS_FILE}: expected {expected}, got {actual}"
    )]
    ChecksumMismatch {
        /// The archive file name.
        file: String,
        /// The digest the manifest lists, as hex.
        expected: String,
        /// The digest of the bytes actually downloaded, as hex.
        actual: String,
    },
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
    /// Downloads the [`CHECKSUMS_FILE`] manifest published with release `tag`.
    fn download_checksums(&self, tag: &str) -> Result<String, ReleaseError>;
    /// Downloads the release archive for `tag` (e.g. `"v0.2.3"`, from a prior
    /// [`Self::latest_release`] call) and `platform` (e.g. `"aarch64-linux"`).
    fn download_archive(&self, tag: &str, platform: &str) -> Result<Vec<u8>, ReleaseError>;
}

/// Fetches releases from a GitHub repository over HTTPS.
pub struct GithubReleaseAdapter {
    agent: ureq::Agent,
    repo: String,
    user_agent: &'static str,
    attempts: u32,
}

impl GithubReleaseAdapter {
    /// Creates an adapter for `repo` (e.g. `"mjonuschat/aldis"`).
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
            repo: repo.into(),
            user_agent: "aldis-self-update",
            attempts: MAX_ATTEMPTS,
        }
    }

    /// Creates an adapter for `repo` that sends `user_agent`, makes a single attempt per request,
    /// and gives up on any request that takes longer than `timeout`, for lookups that must not hold up the command making them.
    pub fn with_timeout(
        repo: impl Into<String>,
        user_agent: &'static str,
        timeout: std::time::Duration,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build();
        Self {
            agent: config.into(),
            repo: repo.into(),
            user_agent,
            attempts: 1,
        }
    }

    /// The tag of the repository's latest release. GitHub never reports a pre-release or a draft
    /// as the latest release.
    pub fn latest_tag(&self) -> Result<String, ReleaseError> {
        #[derive(serde::Deserialize)]
        struct Response {
            tag_name: String,
        }

        let response = with_retries(self.attempts, || {
            Ok(self
                .agent
                .get(format!(
                    "https://api.github.com/repos/{}/releases/latest",
                    self.repo
                ))
                .header("User-Agent", self.user_agent)
                .header("Accept", "application/vnd.github+json")
                .call()?
                .body_mut()
                .read_to_string()?)
        })?;
        let parsed: Response = serde_json::from_str(&response)?;
        Ok(parsed.tag_name)
    }

    /// Downloads release `tag`'s text asset `asset`.
    pub fn download_text(&self, tag: &str, asset: &str) -> Result<String, ReleaseError> {
        with_retries(self.attempts, || {
            Ok(self
                .agent
                .get(asset_url(&self.repo, tag, asset))
                .header("User-Agent", self.user_agent)
                .call()?
                .body_mut()
                .read_to_string()?)
        })
    }

    /// Downloads release `tag`'s asset `asset`, refusing one past [`MAX_ARCHIVE_BYTES`].
    pub fn download_bytes(&self, tag: &str, asset: &str) -> Result<Vec<u8>, ReleaseError> {
        with_retries(self.attempts, || {
            let mut response = self
                .agent
                .get(asset_url(&self.repo, tag, asset))
                .header("User-Agent", self.user_agent)
                .call()?;
            // ureq limits encoded bytes before decompressing. Allow a size probe on the wire,
            // and independently bound the decoded stream that is buffered for verification.
            read_archive(
                response
                    .body_mut()
                    .with_config()
                    .limit(MAX_ARCHIVE_BYTES + 1)
                    .reader(),
            )
        })
    }
}

impl ReleasePort for GithubReleaseAdapter {
    fn latest_release(&self) -> Result<LatestRelease, ReleaseError> {
        Ok(LatestRelease {
            tag: self.latest_tag()?,
        })
    }

    fn download_checksums(&self, tag: &str) -> Result<String, ReleaseError> {
        self.download_text(tag, CHECKSUMS_FILE)
    }

    fn download_archive(&self, tag: &str, platform: &str) -> Result<Vec<u8>, ReleaseError> {
        self.download_bytes(tag, &archive_name(platform))
    }
}

/// Attempts per request, for callers that opt in, before a transient GitHub failure is reported.
const MAX_ATTEMPTS: u32 = 4;

/// Whether `error` is worth retrying: GitHub 5xx/429 responses and network-level failures.
fn is_transient(error: &ReleaseError) -> bool {
    match error {
        ReleaseError::Http(ureq::Error::StatusCode(code)) => *code == 429 || *code >= 500,
        ReleaseError::Http(ureq::Error::BodyExceedsLimit(_)) => false,
        ReleaseError::Http(_) => true,
        _ => false,
    }
}

/// Runs `request`, retrying transient failures (e.g. a 504 from GitHub's CDN) with backoff.
fn with_retries<T>(
    attempts: u32,
    mut request: impl FnMut() -> Result<T, ReleaseError>,
) -> Result<T, ReleaseError> {
    let mut attempt = 1;
    loop {
        match request() {
            Err(error) if attempt < attempts && is_transient(&error) => {
                std::thread::sleep(std::time::Duration::from_secs(1 << (attempt - 1)));
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn read_archive(mut reader: impl Read) -> Result<Vec<u8>, ReleaseError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_ARCHIVE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| map_archive_error(error.into()))?;
    let mut extra = [0_u8; 1];
    let extra_bytes = loop {
        match reader.read(&mut extra) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result.map_err(|error| map_archive_error(error.into()))?,
        }
    };
    if extra_bytes != 0 {
        return Err(ReleaseError::ArchiveTooLarge(MAX_ARCHIVE_BYTES));
    }
    Ok(bytes)
}

/// Names the size cap when the body exceeded it; every other transport failure passes through.
fn map_archive_error(error: ureq::Error) -> ReleaseError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => ReleaseError::ArchiveTooLarge(MAX_ARCHIVE_BYTES),
        error => ReleaseError::Http(error),
    }
}

/// Returns whether `current_version` (e.g. `"0.2.3"`, matching
/// `CARGO_PKG_VERSION`) needs no update: it already matches `latest_tag`
/// (e.g. `"v0.2.3"`), or is ahead of it (e.g. a locally built version not
/// yet published). Either version failing to parse as semver is treated as
/// up to date, refusing to install a release it can't confirm is newer.
pub fn is_up_to_date(current_version: &str, latest_tag: &str) -> bool {
    let (Ok(current), Ok(latest)) = (
        semver::Version::parse(current_version),
        semver::Version::parse(latest_tag.trim_start_matches('v')),
    ) else {
        return true;
    };
    current >= latest
}

/// Maps the host architecture to the platform suffix release archives are
/// published under (`aldis-<platform>.tar.xz`), matching the targets
/// `release.yml` builds.
pub fn target_platform(arch: &str) -> Result<&'static str, ReleaseError> {
    match arch {
        "x86_64" => Ok("x86_64-linux"),
        "aarch64" => Ok("aarch64-linux"),
        "arm" => Ok("armv7l-linux"),
        other => Err(ReleaseError::UnsupportedPlatform(other.to_owned())),
    }
}

/// The archive file name a release publishes for `platform`, matching `release.yml`.
pub fn archive_name(platform: &str) -> String {
    format!("aldis-{platform}.tar.xz")
}

fn asset_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

/// Finds `file_name`'s digest in a [`CHECKSUMS_FILE`] manifest. Each line is as `sha256sum`
/// writes it: 64 hex digits, a space, then a space or `*`, then the file name.
pub fn expected_checksum(manifest: &str, file_name: &str) -> Result<[u8; 32], ReleaseError> {
    let entry = manifest
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(_, name)| name.trim_start_matches([' ', '*']) == file_name)
        .ok_or_else(|| ReleaseError::ChecksumMissing(file_name.to_owned()))?;
    decode_hex(entry.0).ok_or_else(|| ReleaseError::ChecksumInvalid(file_name.to_owned()))
}

/// Checks that `archive`'s SHA-256 digest is `expected`.
pub fn verify_checksum(
    file_name: &str,
    archive: &[u8],
    expected: &[u8; 32],
) -> Result<(), ReleaseError> {
    let actual: [u8; 32] = Sha256::digest(archive).into();
    if actual == *expected {
        Ok(())
    } else {
        Err(ReleaseError::ChecksumMismatch {
            file: file_name.to_owned(),
            expected: encode_hex(expected),
            actual: encode_hex(&actual),
        })
    }
}

fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

fn encode_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
            stdin: None,
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
    use std::io::{self, BufRead, Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::{
        CHECKSUMS_FILE, GithubReleaseAdapter, MAX_ARCHIVE_BYTES, MAX_ATTEMPTS, ReleaseError,
        ReleasePort, archive_name, asset_url, expected_checksum, is_up_to_date, map_archive_error,
        read_archive, target_platform, verify_checksum, with_retries,
    };

    // Both line formats `sha256sum` writes (text mode `  `, binary mode ` *`); the digests are
    // arbitrary, these tests only parse them.
    const MANIFEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  aldis-x86_64-linux.tar.xz\n\
                            d6c4b7c6a3b0d2bb7e3d3ea8bd2b2ea9b3a1dd8ad1f6ce6a3e8a19e5c0f2b1a4 *aldis-aarch64-linux.tar.xz\n";

    #[test]
    fn maps_archive_size_errors_without_hiding_other_http_errors() {
        assert!(matches!(
            map_archive_error(ureq::Error::BodyExceedsLimit(MAX_ARCHIVE_BYTES)),
            ReleaseError::ArchiveTooLarge(limit) if limit == MAX_ARCHIVE_BYTES
        ));
        assert!(matches!(
            map_archive_error(ureq::Error::StatusCode(404)),
            ReleaseError::Http(ureq::Error::StatusCode(404))
        ));
        assert_eq!(
            ReleaseError::ArchiveTooLarge(MAX_ARCHIVE_BYTES).to_string(),
            "the release archive is larger than the 67108864 bytes aldis will download"
        );
    }

    #[test]
    fn accepts_archive_downloads_below_and_at_the_size_cap() {
        for size in [MAX_ARCHIVE_BYTES - 1, MAX_ARCHIVE_BYTES] {
            for (gzip, chunked) in [(false, false), (false, true), (true, false), (true, true)] {
                let bytes = download_test_archive(size, gzip, chunked).unwrap_or_else(|error| {
                    panic!("size={size}, gzip={gzip}, chunked={chunked}: {error}")
                });
                assert_eq!(bytes.len() as u64, size);
                assert!(bytes.iter().all(|byte| *byte == b'x'));
            }
        }
    }

    #[test]
    fn rejects_oversized_archive_downloads_including_gzip_responses() {
        for (gzip, chunked) in [(false, false), (false, true), (true, false), (true, true)] {
            match download_test_archive(MAX_ARCHIVE_BYTES + 1, gzip, chunked) {
                Err(ReleaseError::ArchiveTooLarge(limit)) => assert_eq!(limit, MAX_ARCHIVE_BYTES),
                Err(error) => panic!("gzip={gzip}, chunked={chunked}: {error}"),
                Ok(bytes) => panic!(
                    "accepted {} bytes: gzip={gzip}, chunked={chunked}",
                    bytes.len()
                ),
            }
        }
    }

    #[test]
    fn stops_reading_the_decoded_stream_after_one_byte_past_the_cap() {
        struct CountedReader(u64);

        impl Read for CountedReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let remaining = MAX_ARCHIVE_BYTES + 1 - self.0;
                if remaining == 0 {
                    return Err(io::Error::other("read beyond the size probe"));
                }
                let count = remaining.min(buffer.len() as u64) as usize;
                buffer[..count].fill(b'x');
                self.0 += count as u64;
                Ok(count)
            }
        }

        let mut reader = CountedReader(0);
        assert!(
            matches!(read_archive(&mut reader), Err(ReleaseError::ArchiveTooLarge(limit)) if limit == MAX_ARCHIVE_BYTES)
        );
        assert_eq!(reader.0, MAX_ARCHIVE_BYTES + 1);
    }

    fn download_test_archive(
        size: u64,
        gzip: bool,
        chunked: bool,
    ) -> Result<Vec<u8>, ReleaseError> {
        let body = if gzip {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            io::copy(&mut io::repeat(b'x').take(size), &mut encoder).expect("compress response");
            encoder.finish().expect("finish gzip response")
        } else {
            vec![b'x'; size as usize]
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept download");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .expect("write timeout");
            let mut request = String::new();
            let mut reader = io::BufReader::new(&mut stream);
            while !request.ends_with("\r\n\r\n") {
                assert_ne!(reader.read_line(&mut request).expect("read request"), 0);
            }
            let mut send_response = || -> io::Result<()> {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n"
                )?;
                if gzip {
                    write!(stream, "Content-Encoding: gzip\r\n")?;
                }
                if chunked {
                    write!(stream, "Transfer-Encoding: chunked\r\n\r\n")?;
                    for chunk in body.chunks(8192) {
                        write!(stream, "{:x}\r\n", chunk.len())?;
                        stream.write_all(chunk)?;
                        write!(stream, "\r\n")?;
                    }
                    write!(stream, "0\r\n\r\n")
                } else {
                    write!(stream, "Content-Length: {}\r\n\r\n", body.len())?;
                    stream.write_all(&body)
                }
            };
            if let Err(error) = send_response() {
                assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ),
                    "send response: {error}"
                );
            }
        });
        let uri: ureq::http::Uri = format!("http://{address}/archive")
            .parse()
            .expect("local URI");
        let config = ureq::config::Config::builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .middleware(
                move |mut request: ureq::http::Request<ureq::SendBody>,
                      next: ureq::middleware::MiddlewareNext| {
                    *request.uri_mut() = uri.clone();
                    next.handle(request)
                },
            )
            .build();
        let adapter = GithubReleaseAdapter {
            agent: config.into(),
            repo: "test/aldis".to_owned(),
            user_agent: "aldis-self-update",
            attempts: 1,
        };
        let result = adapter.download_archive("v0.4.1", "aarch64-linux");
        server.join().expect("download server");
        result
    }

    #[test]
    fn retries_transient_gateway_errors_but_not_client_errors() {
        let mut calls = 0;
        let result = with_retries(MAX_ATTEMPTS, || {
            calls += 1;
            if calls < 3 {
                Err(ReleaseError::Http(ureq::Error::StatusCode(504)))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(result.unwrap(), 3);

        let mut calls = 0;
        let result: Result<(), _> = with_retries(MAX_ATTEMPTS, || {
            calls += 1;
            Err(ReleaseError::Http(ureq::Error::StatusCode(404)))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn matches_a_tag_with_or_without_its_v_prefix() {
        assert!(is_up_to_date("0.2.3", "v0.2.3"));
        assert!(!is_up_to_date("0.2.2", "v0.2.3"));
    }

    #[test]
    fn downloads_the_assets_for_the_specific_tag_rather_than_whatever_is_latest() {
        assert_eq!(
            asset_url("mjonuschat/aldis", "v0.2.3", &archive_name("aarch64-linux")),
            "https://github.com/mjonuschat/aldis/releases/download/v0.2.3/aldis-aarch64-linux.tar.xz"
        );
        assert_eq!(
            asset_url("mjonuschat/aldis", "v0.2.3", CHECKSUMS_FILE),
            "https://github.com/mjonuschat/aldis/releases/download/v0.2.3/SHA256SUMS"
        );
    }

    #[test]
    fn finds_the_archives_digest_in_either_sha256sum_line_format() {
        let aarch64 = expected_checksum(MANIFEST, "aldis-aarch64-linux.tar.xz").unwrap();
        assert_eq!(aarch64[0], 0xd6);
        assert_eq!(aarch64[31], 0xa4);
        let x86_64 = expected_checksum(MANIFEST, "aldis-x86_64-linux.tar.xz").unwrap();
        assert_eq!(x86_64[0], 0x01);
        assert_eq!(x86_64[31], 0xef);
    }

    #[test]
    fn refuses_a_manifest_without_the_archive_or_with_a_malformed_digest() {
        assert!(matches!(
            expected_checksum(MANIFEST, "aldis-armv7l-linux.tar.xz"),
            Err(ReleaseError::ChecksumMissing(file)) if file == "aldis-armv7l-linux.tar.xz"
        ));
        // A name that merely ends with the archive name must not match.
        assert!(matches!(
            expected_checksum(MANIFEST, "x86_64-linux.tar.xz"),
            Err(ReleaseError::ChecksumMissing(_))
        ));
        for manifest in [
            "abc  aldis-x86_64-linux.tar.xz\n",
            "zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  aldis-x86_64-linux.tar.xz\n",
        ] {
            assert!(matches!(
                expected_checksum(manifest, "aldis-x86_64-linux.tar.xz"),
                Err(ReleaseError::ChecksumInvalid(_))
            ));
        }
        assert!(matches!(
            expected_checksum("", "aldis-x86_64-linux.tar.xz"),
            Err(ReleaseError::ChecksumMissing(_))
        ));
    }

    #[test]
    fn accepts_an_archive_whose_digest_matches_and_names_both_digests_otherwise() {
        let expected = expected_checksum(
            &format!("{}  aldis.tar.xz\n", sha256_hex(b"firmware")),
            "aldis.tar.xz",
        )
        .unwrap();
        assert!(verify_checksum("aldis.tar.xz", b"firmware", &expected).is_ok());

        let error = verify_checksum("aldis.tar.xz", b"firmwarf", &expected).unwrap_err();
        match &error {
            ReleaseError::ChecksumMismatch {
                file,
                expected: listed,
                actual,
            } => {
                assert_eq!(file, "aldis.tar.xz");
                assert_eq!(*listed, sha256_hex(b"firmware"));
                assert_eq!(*actual, sha256_hex(b"firmwarf"));
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
        assert!(
            error
                .to_string()
                .starts_with("downloaded aldis.tar.xz does not match the release's SHA256SUMS: ")
        );
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn does_not_offer_to_downgrade_a_locally_built_version_ahead_of_the_release() {
        assert!(is_up_to_date("0.3.0", "v0.2.3"));
        assert!(is_up_to_date("1.0.0", "v0.2.3"));
    }

    #[test]
    fn compares_versions_numerically_rather_than_lexically() {
        assert!(!is_up_to_date("0.9.0", "v0.10.0"));
        assert!(is_up_to_date("0.10.0", "v0.9.0"));
    }

    #[test]
    fn refuses_to_update_when_either_version_does_not_parse_as_semver() {
        assert!(is_up_to_date("0.2.3", "v0.2.3-not-a-real-tag!"));
        assert!(is_up_to_date("not-a-version", "v0.2.3"));
    }

    #[test]
    fn maps_supported_host_architectures_to_release_platform_names() {
        assert_eq!(target_platform("x86_64").unwrap(), "x86_64-linux");
        assert_eq!(target_platform("aarch64").unwrap(), "aarch64-linux");
        assert_eq!(target_platform("arm").unwrap(), "armv7l-linux");
        assert!(target_platform("riscv64").is_err());
    }
}
