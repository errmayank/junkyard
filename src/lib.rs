mod error;

#[cfg(target_os = "macos")]
mod macos;
mod util;

pub use error::{Error, Result};

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Describes an item moved to the system trash.
#[derive(Clone, Debug)]
pub struct TrashItem {
    id: OsString,
    name: OsString,
    original_parent: PathBuf,
    discarded_at: SystemTime,
}

impl TrashItem {
    #[must_use]
    pub(crate) fn new(
        id: OsString,
        name: OsString,
        original_parent: PathBuf,
        discarded_at: SystemTime,
    ) -> Self {
        Self {
            id,
            name,
            original_parent,
            discarded_at,
        }
    }

    /// Returns the platform-specific identifier for the trashed item.
    ///
    /// On macOS, this is the filesystem representation of the URL returned by
    /// `NSFileManager::trashItemAtURL`.
    pub fn id(&self) -> &OsStr {
        &self.id
    }

    /// Returns the trashed item's original file name.
    ///
    /// On macOS, for `/Users/me/Downloads/file.txt`, this returns `file.txt`.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Returns the directory that originally contained the trashed item.
    ///
    /// The parent directory is canonicalized, so the returned value may be different from
    /// the parent of the path passed to [`Trash::discard`].
    ///
    /// On macOS:
    ///
    /// - For `/Users/me/Downloads/file.txt`, this returns `/Users/me/Downloads`.
    /// - For `/var/folders/example/file.txt`, this returns `/private/var/folders/example`.
    pub fn original_parent(&self) -> &Path {
        &self.original_parent
    }

    /// Returns the trashed item's original full path.
    ///
    /// This is equivalent to joining [`TrashItem::original_parent`] and [`TrashItem::name`].
    ///
    /// The parent directory is canonicalized, so the returned value may be different from
    /// the path passed to [`Trash::discard`].
    ///
    /// On macOS:
    ///
    /// - For `/Users/me/Downloads/file.txt`, this returns `/Users/me/Downloads/file.txt`.
    /// - For `/var/folders/example/file.txt`, this returns `/private/var/folders/example/file.txt`.
    #[must_use]
    pub fn original_path(&self) -> PathBuf {
        self.original_parent.join(&self.name)
    }

    /// Returns when the item was moved to the trash.
    ///
    /// On macOS, this is recorded immediately after `NSFileManager` reports success.
    #[must_use]
    pub fn discarded_at(&self) -> SystemTime {
        self.discarded_at
    }
}

/// Provides access to system trash operations.
#[derive(Clone, Copy, Debug, Default)]
pub struct Trash;

impl Trash {
    /// Creates a trash handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Moves a single path to the system trash.
    ///
    /// Symbolic links are moved as links; their targets are left in place.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be resolved or moved to the system trash.
    pub fn discard<P>(&self, path: P) -> Result<TrashItem>
    where
        P: AsRef<Path>,
    {
        let path = util::resolve_path(path.as_ref())?;

        #[cfg(target_os = "macos")]
        {
            macos::discard(self, &path)
        }

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            std::mem::drop(path);
            unimplemented!()
        }
    }

    /// Moves multiple paths to the system trash.
    ///
    /// Returns one [`TrashItem`] per path, in input order.
    ///
    /// Symbolic links are moved as links; their targets are left in place.
    ///
    /// All paths are resolved before any item is moved to the trash. If resolution
    /// fails, no items are moved. Once trashing begins, paths are processed in input
    /// order. If a later operation fails, earlier items may already be in the trash.
    ///
    /// # Errors
    ///
    /// Returns an error if any path cannot be resolved or moved to the system trash.
    pub fn discard_all<I, P>(&self, paths: I) -> Result<Vec<TrashItem>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| util::resolve_path(path.as_ref()))
            .collect::<Result<Vec<_>>>()?;

        #[cfg(target_os = "macos")]
        {
            macos::discard_all(self, &paths)
        }

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            std::mem::drop(paths);
            unimplemented!()
        }
    }
}
