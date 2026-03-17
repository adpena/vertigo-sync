//! Structured error catalog with user-friendly messages and suggestions.

use std::fmt;
use std::path::PathBuf;

/// Structured sync errors with human-readable explanations and actionable suggestions.
#[derive(Debug)]
pub enum SyncError {
    /// No project file found at the expected path.
    ProjectNotFound { path: PathBuf },
    /// The requested port is already in use.
    PortInUse { port: u16 },
    /// Failed to read a snapshot file.
    SnapshotReadFailed { path: PathBuf },
    /// Two successive snapshots produced different hashes.
    NonDeterministic {
        hash1: String,
        hash2: String,
    },
    /// Source validation found blocking errors.
    ValidationFailed { errors: usize, warnings: usize },
    /// Plugin directory could not be located.
    PluginDirNotFound,
    /// The include root does not exist on disk.
    IncludeRootMissing { path: PathBuf },
}

impl SyncError {
    /// Short, one-line title for the error.
    pub fn title(&self) -> &str {
        match self {
            Self::ProjectNotFound { .. } => "Project file not found",
            Self::PortInUse { .. } => "Port already in use",
            Self::SnapshotReadFailed { .. } => "Snapshot read failed",
            Self::NonDeterministic { .. } => "Non-deterministic snapshots",
            Self::ValidationFailed { .. } => "Validation failed",
            Self::PluginDirNotFound => "Plugin directory not found",
            Self::IncludeRootMissing { .. } => "Include root missing",
        }
    }

    /// Detailed explanation of what went wrong.
    pub fn explanation(&self) -> String {
        match self {
            Self::ProjectNotFound { path } => {
                format!(
                    "Expected a project file at '{}' but none was found.",
                    path.display()
                )
            }
            Self::PortInUse { port } => {
                format!(
                    "Port {port} is already bound by another process. \
                     Vertigo Sync cannot start the HTTP server."
                )
            }
            Self::SnapshotReadFailed { path } => {
                format!(
                    "Could not read or parse the snapshot file at '{}'.",
                    path.display()
                )
            }
            Self::NonDeterministic { hash1, hash2 } => {
                format!(
                    "Two consecutive snapshots produced different fingerprints:\n  \
                     1: {hash1}\n  \
                     2: {hash2}\n\
                     This indicates non-deterministic file ordering or content."
                )
            }
            Self::ValidationFailed { errors, warnings } => {
                format!(
                    "Source validation found {errors} error(s) and {warnings} warning(s)."
                )
            }
            Self::PluginDirNotFound => {
                "Could not locate the Roblox Studio plugins directory for this OS.".to_string()
            }
            Self::IncludeRootMissing { path } => {
                format!(
                    "The include root '{}' does not exist on disk.",
                    path.display()
                )
            }
        }
    }

    /// Actionable suggestion for the user.
    pub fn suggestion(&self) -> String {
        match self {
            Self::ProjectNotFound { .. } => {
                "Run 'vertigo-sync init' to create a new project, \
                 or use --root to point to your project directory."
                    .to_string()
            }
            Self::PortInUse { port } => {
                format!(
                    "Stop the process using port {port}, or use --port to choose a different port."
                )
            }
            Self::SnapshotReadFailed { .. } => {
                "Run 'vertigo-sync snapshot' first to generate a fresh snapshot.".to_string()
            }
            Self::NonDeterministic { .. } => {
                "Check for timestamp-dependent or random content in your source files. \
                 Run 'vertigo-sync doctor' for details."
                    .to_string()
            }
            Self::ValidationFailed { .. } => {
                "Fix the reported errors and run 'vertigo-sync validate' again.".to_string()
            }
            Self::PluginDirNotFound => {
                "Ensure Roblox Studio is installed. On macOS the plugins directory is \
                 ~/Documents/Roblox/Plugins/."
                    .to_string()
            }
            Self::IncludeRootMissing { path } => {
                format!(
                    "Create the directory '{}' or adjust your --include paths.",
                    path.display()
                )
            }
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.title(), self.explanation())
    }
}

impl std::error::Error for SyncError {}
