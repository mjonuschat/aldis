//! Local Klipper checkout revision discovery.

use std::path::Path;

use git2::{DescribeFormatOptions, DescribeOptions, Repository, StatusOptions};

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
}

/// Returns a clean checkout revision, or an indeterminate state for dirty worktrees.
pub fn revision(path: &Path) -> Result<CheckoutRevision, CheckoutError> {
    let repository = Repository::open(path).map_err(CheckoutError::Repository)?;
    let mut statuses = StatusOptions::new();
    statuses
        .include_untracked(true)
        .recurse_untracked_dirs(true);
    if !repository
        .statuses(Some(&mut statuses))
        .map_err(CheckoutError::Status)?
        .is_empty()
    {
        return Ok(CheckoutRevision::Indeterminate);
    }
    let mut describe = DescribeOptions::new();
    describe.describe_tags().show_commit_oid_as_fallback(true);
    let description = repository
        .describe(&describe)
        .map_err(CheckoutError::Describe)?;
    let mut format = DescribeFormatOptions::new();
    format.always_use_long_format(true).abbreviated_size(8);
    let revision = description
        .format(Some(&format))
        .map_err(CheckoutError::Describe)?;
    Ok(CheckoutRevision::Known(revision))
}
