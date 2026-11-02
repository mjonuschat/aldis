//! Local Klipper checkout revision discovery.

use std::path::Path;
use std::process::{Command, Output};

use crate::eligibility::CheckoutRevision;

/// Failure while examining a selected Klipper checkout.
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    /// The `git` binary could not be executed.
    #[error("could not run git")]
    Spawn(#[source] std::io::Error),
    /// A `git` invocation against the checkout failed.
    #[error("could not {action} the Klipper checkout: {stderr}")]
    Command {
        /// The operation that failed.
        action: &'static str,
        /// `git`'s own error output.
        stderr: String,
    },
    /// The checked-out branch does not name an upstream to refresh from.
    #[error("the checked-out Klipper branch has no configured upstream")]
    UpstreamNotConfigured,
    /// The fetched upstream cannot be applied without a merge or rebase.
    #[error(
        "cannot fast-forward {branch} from {upstream}; resolve the divergence before updating firmware"
    )]
    NotFastForward {
        /// The current local branch name.
        branch: String,
        /// The configured upstream branch name.
        upstream: String,
    },
}

/// A source of Klipper checkout state and fast-forward refreshes.
///
/// Isolates callers that only need checkout revisions and refreshes from the
/// concrete `git`-backed implementation, so they can be tested against a fake.
pub trait CheckoutPort {
    /// Returns the checkout revision, including a dirty suffix for tracked changes.
    fn revision(&self, path: &Path) -> Result<CheckoutRevision, CheckoutError>;

    /// Fetches the checked-out branch's configured upstream and applies only a fast-forward.
    fn refresh(&self, path: &Path) -> Result<RefreshResult, CheckoutError>;
}

/// A [`CheckoutPort`] backed by the local Git checkout through the system `git` binary.
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

fn git(path: &Path, action: &'static str, args: &[&str]) -> Result<String, CheckoutError> {
    let output: Output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(CheckoutError::Spawn)?;
    if !output.status.success() {
        return Err(CheckoutError::Command {
            action,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Runs a `git` query that answers yes or no through its exit status: 0 is yes, 1 is no, and
/// anything else (a missing repository, an unknown ref, a failed spawn) is an error rather than
/// a no.
fn git_check(path: &Path, action: &'static str, args: &[&str]) -> Result<bool, CheckoutError> {
    let output: Output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(CheckoutError::Spawn)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CheckoutError::Command {
            action,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
    }
}

/// Returns the checkout revision, including a dirty suffix for tracked changes.
pub fn revision(path: &Path) -> Result<CheckoutRevision, CheckoutError> {
    let description = git(
        path,
        "describe",
        &["describe", "--tags", "--long", "--always", "--dirty"],
    )?;
    Ok(CheckoutRevision::Known(description))
}

/// Fetches the checked-out branch's configured upstream and applies only a fast-forward.
///
/// Local modifications are retained when they do not conflict with the upstream tree. A
/// divergent branch is left unchanged so the operator can resolve it with their normal Git
/// workflow before any firmware operation begins.
pub fn refresh(path: &Path) -> Result<RefreshResult, CheckoutError> {
    let before = revision(path)?;

    let branch = git(
        path,
        "read the checked-out branch",
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )?;
    let upstream = git(
        path,
        "read the configured upstream",
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .map_err(|_| CheckoutError::UpstreamNotConfigured)?;
    let remote = upstream.split('/').next().unwrap_or_default().to_owned();

    git(path, "fetch the configured upstream", &["fetch", &remote])?;

    let is_ancestor = git_check(
        path,
        "check whether the checkout can fast-forward",
        &["merge-base", "--is-ancestor", "HEAD", &upstream],
    )?;
    if !is_ancestor {
        return Err(CheckoutError::NotFastForward { branch, upstream });
    }

    let ahead_behind = git(
        path,
        "count fetched commits",
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{upstream}"),
        ],
    )?;
    let commits_advanced: usize = ahead_behind
        .split_whitespace()
        .nth(1)
        .and_then(|behind| behind.parse().ok())
        .ok_or(CheckoutError::Command {
            action: "count fetched commits",
            stderr: format!("unexpected `git rev-list` output: {ahead_behind}"),
        })?;

    if commits_advanced == 0 {
        return Ok(RefreshResult {
            before: before.clone(),
            after: before,
            advanced: false,
            commits_advanced: 0,
        });
    }

    git(
        path,
        "apply the fast-forward without overwriting local changes",
        &["merge", "--ff-only", &upstream],
    )?;
    let after = revision(path)?;
    Ok(RefreshResult {
        before,
        after,
        advanced: true,
        commits_advanced,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{CheckoutError, git_check};

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("LC_ALL", "C")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn tells_a_negative_answer_apart_from_a_failed_git_command() {
        let dir = tempfile::tempdir().unwrap();
        let identity = [
            "-c",
            "user.name=aldis",
            "-c",
            "user.email=aldis@example.invalid",
        ];
        git(dir.path(), &["init", "-q"]);
        for message in ["one", "two"] {
            let mut args = identity.to_vec();
            args.extend(["commit", "-q", "--allow-empty", "-m", message]);
            git(dir.path(), &args);
        }
        let ancestry = |older: &str, newer: &str| {
            git_check(
                dir.path(),
                "check ancestry",
                &["merge-base", "--is-ancestor", older, newer],
            )
        };

        assert!(matches!(ancestry("HEAD~1", "HEAD"), Ok(true)));
        assert!(matches!(ancestry("HEAD", "HEAD~1"), Ok(false)));
        assert!(matches!(
            ancestry("HEAD", "no-such-ref"),
            Err(CheckoutError::Command {
                action: "check ancestry",
                ..
            })
        ));
        assert!(matches!(
            git_check(
                Path::new("/nonexistent/aldis-checkout"),
                "check ancestry",
                &["merge-base", "--is-ancestor", "HEAD", "HEAD"],
            ),
            Err(CheckoutError::Command { .. })
        ));
    }
}
