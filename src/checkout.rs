//! Local Klipper checkout revision discovery.

use std::path::Path;

use git2::{
    BranchType, DescribeFormatOptions, DescribeOptions, FetchOptions, Repository, StatusOptions,
    build::CheckoutBuilder,
};

use crate::eligibility::CheckoutRevision;

/// Failure while examining a selected Klipper checkout.
#[derive(Debug)]
pub enum CheckoutError {
    /// The path is not an accessible Git repository.
    Repository(git2::Error),
    /// The checkout revision could not be described.
    Describe(git2::Error),
    /// Worktree state could not be checked.
    Status(git2::Error),
    /// The checked-out branch does not name an upstream to refresh from.
    UpstreamNotConfigured,
    /// The fetched upstream cannot be applied without a merge or rebase.
    NotFastForward {
        /// The current local branch name.
        branch: String,
        /// The configured upstream branch name.
        upstream: String,
    },
    /// Fetching, checking out, or updating the configured upstream failed.
    Refresh {
        /// The refresh operation that failed.
        action: &'static str,
        /// The underlying libgit2 error.
        source: git2::Error,
    },
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repository(error) => {
                write!(formatter, "could not open the Klipper checkout: {error}")
            }
            Self::Describe(error) => write!(
                formatter,
                "could not describe the Klipper checkout: {error}"
            ),
            Self::Status(error) => {
                write!(formatter, "could not inspect the Klipper checkout: {error}")
            }
            Self::UpstreamNotConfigured => write!(
                formatter,
                "the checked-out Klipper branch has no configured upstream"
            ),
            Self::NotFastForward { branch, upstream } => write!(
                formatter,
                "cannot fast-forward {branch} from {upstream}; resolve the divergence before updating firmware"
            ),
            Self::Refresh { action, source } => {
                write!(
                    formatter,
                    "could not {action} the Klipper checkout: {source}"
                )
            }
        }
    }
}

impl std::error::Error for CheckoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Repository(error) | Self::Describe(error) | Self::Status(error) => Some(error),
            Self::Refresh { source, .. } => Some(source),
            Self::UpstreamNotConfigured | Self::NotFastForward { .. } => None,
        }
    }
}

/// A source of Klipper checkout state and fast-forward refreshes.
///
/// Isolates callers that only need checkout revisions and refreshes from the
/// concrete `git2`-backed implementation, so they can be tested against a fake.
pub trait CheckoutPort {
    /// Returns the checkout revision, including a dirty suffix for tracked changes.
    fn revision(&self, path: &Path) -> Result<CheckoutRevision, CheckoutError>;

    /// Fetches the checked-out branch's configured upstream and applies only a fast-forward.
    fn refresh(&self, path: &Path) -> Result<RefreshResult, CheckoutError>;
}

/// A [`CheckoutPort`] backed by the local Git checkout through `libgit2`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitCheckoutAdapter;

impl CheckoutPort for GitCheckoutAdapter {
    fn revision(&self, path: &Path) -> Result<CheckoutRevision, CheckoutError> {
        revision(path)
    }

    fn refresh(&self, path: &Path) -> Result<RefreshResult, CheckoutError> {
        refresh(path)
    }
}

/// A successful fast-forward-only source refresh.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshResult {
    /// The checkout revision before fetching the configured upstream.
    pub before: CheckoutRevision,
    /// The checkout revision after completing the refresh.
    pub after: CheckoutRevision,
    /// Whether the local branch advanced to a newer commit.
    pub advanced: bool,
    /// Number of commits incorporated from the configured upstream.
    pub commits_advanced: usize,
}

/// Returns the checkout revision, including a dirty suffix for tracked changes.
pub fn revision(path: &Path) -> Result<CheckoutRevision, CheckoutError> {
    let repository = Repository::open(path).map_err(CheckoutError::Repository)?;
    let mut statuses = StatusOptions::new();
    statuses
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    let dirty = !repository
        .statuses(Some(&mut statuses))
        .map_err(CheckoutError::Status)?
        .is_empty();
    let mut describe = DescribeOptions::new();
    describe.describe_tags().show_commit_oid_as_fallback(true);
    let description = repository
        .describe(&describe)
        .map_err(CheckoutError::Describe)?;
    // This abbreviation length need not match the one Klipper's own `git describe`
    // (no --abbrev override) picks when it embeds a version in firmware: libgit2 has
    // no equivalent to git's size-based "auto" abbreviation heuristic, so any fixed
    // length can differ. Comparisons against a running MCU's reported version use
    // `eligibility::revisions_match`, which tolerates that by prefix rather than
    // requiring the hash abbreviations to be the same length.
    let mut format = DescribeFormatOptions::new();
    format.always_use_long_format(true).abbreviated_size(8);
    let revision = description
        .format(Some(&format))
        .map_err(CheckoutError::Describe)?;
    let revision = if dirty {
        format!("{revision}-dirty")
    } else {
        revision
    };
    Ok(CheckoutRevision::Known(revision))
}

/// Fetches the checked-out branch's configured upstream and applies only a fast-forward.
///
/// Local modifications are retained when they do not conflict with the upstream tree. A
/// divergent branch is left unchanged so the operator can resolve it with their normal Git
/// workflow before any firmware operation begins.
pub fn refresh(path: &Path) -> Result<RefreshResult, CheckoutError> {
    let repository = Repository::open(path).map_err(CheckoutError::Repository)?;
    let before = revision(path)?;
    let head = repository.head().map_err(|source| CheckoutError::Refresh {
        action: "read the checked-out branch",
        source,
    })?;
    let head_id = head.target().ok_or_else(|| CheckoutError::Refresh {
        action: "resolve the checked-out commit",
        source: git2::Error::from_str("HEAD does not point to a commit"),
    })?;
    let branch_name = head
        .shorthand()
        .map_err(|_| CheckoutError::UpstreamNotConfigured)?
        .to_owned();
    let branch = repository
        .find_branch(&branch_name, BranchType::Local)
        .map_err(|source| CheckoutError::Refresh {
            action: "read the checked-out branch",
            source,
        })?;
    let upstream = branch
        .upstream()
        .map_err(|_| CheckoutError::UpstreamNotConfigured)?;
    let upstream_name = upstream
        .name()
        .map_err(|_| CheckoutError::UpstreamNotConfigured)?
        .ok_or(CheckoutError::UpstreamNotConfigured)?
        .to_owned();
    let upstream_reference_name = upstream
        .get()
        .name()
        .map_err(|_| CheckoutError::UpstreamNotConfigured)?
        .to_owned();
    let remote_name = repository
        .config()
        .and_then(|config| config.get_string(&format!("branch.{branch_name}.remote")))
        .map_err(|_| CheckoutError::UpstreamNotConfigured)?;
    let mut remote =
        repository
            .find_remote(&remote_name)
            .map_err(|source| CheckoutError::Refresh {
                action: "open the configured upstream remote",
                source,
            })?;
    let mut fetch_options = FetchOptions::new();
    remote
        .fetch(&[] as &[&str], Some(&mut fetch_options), None)
        .map_err(|source| CheckoutError::Refresh {
            action: "fetch the configured upstream",
            source,
        })?;

    let upstream_reference = repository
        .find_reference(&upstream_reference_name)
        .map_err(|source| CheckoutError::Refresh {
            action: "read the fetched upstream revision",
            source,
        })?;
    let upstream_commit = repository
        .reference_to_annotated_commit(&upstream_reference)
        .map_err(|source| CheckoutError::Refresh {
            action: "resolve the fetched upstream revision",
            source,
        })?;
    let (analysis, _) = repository
        .merge_analysis(&[&upstream_commit])
        .map_err(|source| CheckoutError::Refresh {
            action: "check whether the upstream can fast-forward",
            source,
        })?;
    if analysis.is_up_to_date() {
        return Ok(RefreshResult {
            before: before.clone(),
            after: before,
            advanced: false,
            commits_advanced: 0,
        });
    }
    if !analysis.is_fast_forward() {
        return Err(CheckoutError::NotFastForward {
            branch: branch_name,
            upstream: upstream_name,
        });
    }

    let commit = repository
        .find_commit(upstream_commit.id())
        .map_err(|source| CheckoutError::Refresh {
            action: "read the fetched upstream commit",
            source,
        })?;
    let (_, commits_advanced) = repository
        .graph_ahead_behind(head_id, commit.id())
        .map_err(|source| CheckoutError::Refresh {
            action: "count fetched commits",
            source,
        })?;
    repository
        .checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().safe()))
        .map_err(|source| CheckoutError::Refresh {
            action: "apply the fast-forward without overwriting local changes",
            source,
        })?;
    let branch_reference = format!("refs/heads/{branch_name}");
    repository
        .find_reference(&branch_reference)
        .and_then(|mut reference| reference.set_target(commit.id(), "aldis fast-forward"))
        .map_err(|source| CheckoutError::Refresh {
            action: "advance the local Klipper branch",
            source,
        })?;
    repository
        .set_head(&branch_reference)
        .map_err(|source| CheckoutError::Refresh {
            action: "update the checked-out Klipper branch",
            source,
        })?;
    let after = revision(path)?;
    Ok(RefreshResult {
        before,
        after,
        advanced: true,
        commits_advanced,
    })
}
