mod error;
mod platform;
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
    original_name: OsString,
    original_parent: PathBuf,
    discarded_at: SystemTime,
}

impl TrashItem {
    #[must_use]
    pub(crate) fn new(
        id: OsString,
        original_name: OsString,
        original_parent: PathBuf,
        discarded_at: SystemTime,
    ) -> Self {
        Self {
            id,
            original_name,
            original_parent,
            discarded_at,
        }
    }

    /// Returns the platform-specific identifier for the trashed item.
    ///
    /// On Linux:
    ///
    /// - This is the absolute path to the `.trashinfo` file.
    /// - For example: `/home/me/.local/share/Trash/info/file.txt.trashinfo`.
    ///
    /// On macOS:
    ///
    /// - This is the filesystem representation of the URL returned by
    ///   `NSFileManager::trashItemAtURL`.
    /// - For example: `/Users/me/.Trash/file.txt`.
    ///
    /// On Windows:
    ///
    /// - This is the recycled item path returned by the Shell.
    /// - For example: `C:\$Recycle.Bin\S-1-5-21-...\$RABC123.txt`.
    pub fn id(&self) -> &OsStr {
        &self.id
    }

    /// Returns the trashed item's original file name.
    ///
    /// On Linux:
    ///
    /// - For `/home/me/Downloads/file.txt`, this returns `file.txt`.
    ///
    /// On macOS:
    ///
    /// - For `/Users/me/Downloads/file.txt`, this returns `file.txt`.
    ///
    /// On Windows:
    ///
    /// - For `C:\Users\me\Downloads\file.txt`, this returns `file.txt`.
    pub fn original_name(&self) -> &OsStr {
        &self.original_name
    }

    /// Returns the directory that originally contained the trashed item.
    ///
    /// On Linux and macOS, the parent directory is canonicalized.
    ///
    /// On Windows, this is the original location recorded by the Shell.
    /// Short 8.3 file names may be returned in their long form.
    ///
    /// On Linux:
    ///
    /// - For `/home/me/Downloads/file.txt`, this returns `/home/me/Downloads`.
    ///
    /// On macOS:
    ///
    /// - For `/Users/me/Downloads/file.txt`, this returns `/Users/me/Downloads`.
    /// - For `/var/folders/example/file.txt`, this returns `/private/var/folders/example`.
    ///
    /// On Windows:
    ///
    /// - For `C:\Users\me\Desktop\file.txt`, this returns `C:\Users\me\Desktop`.
    /// - For `C:\Users\me\DOWNLO~1\file.txt`, this may return `C:\Users\me\Downloads`.
    pub fn original_parent(&self) -> &Path {
        &self.original_parent
    }

    /// Returns the trashed item's original full path.
    ///
    /// This is equivalent to joining [`TrashItem::original_parent`] and
    /// [`TrashItem::original_name`].
    ///
    /// On Linux and macOS, the parent directory is canonicalized.
    ///
    /// On Windows, this uses the original location recorded by the Shell.
    /// Short 8.3 file names may be returned in their long form.
    ///
    /// On Linux:
    ///
    /// - For `/home/me/Downloads/file.txt`, this returns `/home/me/Downloads/file.txt`.
    ///
    /// On macOS:
    ///
    /// - For `/Users/me/Downloads/file.txt`, this returns `/Users/me/Downloads/file.txt`.
    /// - For `/var/folders/example/file.txt`, this returns `/private/var/folders/example/file.txt`.
    ///
    /// On Windows:
    ///
    /// - For `C:\Users\me\Desktop\file.txt`, this returns `C:\Users\me\Desktop\file.txt`.
    /// - For `C:\Users\me\DOWNLO~1\file.txt`, this may return `C:\Users\me\Downloads\file.txt`.
    #[must_use]
    pub fn original_path(&self) -> PathBuf {
        self.original_parent.join(&self.original_name)
    }

    /// Returns the time at which the item was trashed.
    ///
    /// On Linux:
    ///
    /// - This is the timestamp written to the `.trashinfo` file.
    ///
    /// On macOS:
    ///
    /// - This is recorded after the system trash operation succeeds.
    ///
    /// On Windows:
    ///
    /// - This is recorded after the system recycle operation succeeds.
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

        platform::discard(self, &path)
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

        platform::discard_all(self, &paths)
    }
}
