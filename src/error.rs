use std::{io, path::PathBuf};

/// Result type used by junkyard operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by junkyard operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The operation was requested on an empty path.
    #[error("Path is empty")]
    EmptyPath,

    /// The path resolves to a filesystem root, which cannot be trashed.
    #[error("Cannot move filesystem root to trash: {path}")]
    TargetedRoot { path: PathBuf },

    /// An I/O error occurred while preparing or moving the path to the trash.
    #[error("I/O failure for path: {path}")]
    Io {
        path: PathBuf,

        #[source]
        source: io::Error,
    },

    /// The platform rejected the trash operation.
    #[error("Trash operation failed for {path}: {message}")]
    Platform { path: PathBuf, message: String },
}
