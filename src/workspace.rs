use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A caller-chosen, isolated directory for one updater run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkspace {
    root: PathBuf,
}

/// Temporary paths reserved for one MCU firmware build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBuildPaths {
    /// Klipper's generated Kconfig path.
    pub config_path: PathBuf,
    /// Destination for the copied firmware artifact.
    pub artifact_path: PathBuf,
}

/// Failure while creating an isolated run workspace.
#[derive(Debug)]
pub enum WorkspaceError {
    /// The requested run directory already exists.
    AlreadyExists(PathBuf),
    /// The host filesystem operation failed.
    Io {
        /// The operation being attempted.
        action: &'static str,
        /// The underlying failure.
        source: io::Error,
    },
    /// The selected MCU name cannot identify a workspace target.
    EmptyTargetName,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists(path) => write!(
                formatter,
                "run workspace {} already exists; choose a new per-run directory",
                path.display()
            ),
            Self::Io { action, source } => write!(formatter, "could not {action}: {source}"),
            Self::EmptyTargetName => write!(formatter, "MCU target name must not be empty"),
        }
    }
}

impl StdError for WorkspaceError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyExists(_) | Self::EmptyTargetName => None,
        }
    }
}

impl RunWorkspace {
    /// Creates an empty, previously unused workspace at `root`.
    pub fn create(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        if root.exists() {
            return Err(WorkspaceError::AlreadyExists(root));
        }
        fs::create_dir_all(&root).map_err(|source| WorkspaceError::Io {
            action: "create the run workspace",
            source,
        })?;

        Ok(Self { root })
    }

    /// Wraps a directory the caller already created uniquely and atomically
    /// (e.g. via `tempfile`'s `mkdtemp`), skipping [`Self::create`]'s
    /// pre-existence check since it would always fail here.
    pub fn adopt(root: PathBuf) -> Self {
        Self { root }
    }

    /// Returns the workspace root retained for this run's artifacts and logs.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reserves separate generated-config and artifact paths for `target_name`.
    pub fn build_paths(&self, target_name: &str) -> Result<WorkspaceBuildPaths, WorkspaceError> {
        if target_name.is_empty() {
            return Err(WorkspaceError::EmptyTargetName);
        }
        let target_id = target_name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let config_dir = self.root.join("targets").join(&target_id);
        let artifact_dir = self.root.join("artifacts").join(&target_id);
        fs::create_dir_all(&config_dir).map_err(|source| WorkspaceError::Io {
            action: "create the target configuration directory",
            source,
        })?;
        fs::create_dir_all(&artifact_dir).map_err(|source| WorkspaceError::Io {
            action: "create the artifact directory",
            source,
        })?;

        Ok(WorkspaceBuildPaths {
            config_path: config_dir.join(".config"),
            artifact_path: artifact_dir.join("klipper.bin"),
        })
    }
}
