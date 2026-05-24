use objc2_foundation::{NSFileManager, NSURL};
use std::{
    ffi::{CStr, CString, c_char},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::NonNull,
    time::SystemTime,
};

use crate::{Error, Result, Trash, TrashItem};

pub(crate) fn discard(_: &Trash, path: &Path) -> Result<TrashItem> {
    let file_manager = NSFileManager::defaultManager();

    discard_inner(&file_manager, path)
}

pub(crate) fn discard_all(_: &Trash, paths: &[PathBuf]) -> Result<Vec<TrashItem>> {
    let file_manager = NSFileManager::defaultManager();

    paths
        .iter()
        .map(|path| discard_inner(&file_manager, path))
        .collect()
}

fn discard_inner(file_manager: &NSFileManager, path: &Path) -> Result<TrashItem> {
    let path_cstring = CString::new(path.as_os_str().as_bytes()).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, source),
    })?;
    let file_type = std::fs::symlink_metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .file_type();
    let Some(first_byte) = path_cstring.as_bytes_with_nul().first() else {
        return Err(Error::Platform {
            message: format!(
                "Filesystem path representation was empty for {}",
                path.display()
            ),
        });
    };
    let path_ptr = NonNull::from(first_byte).cast::<c_char>();

    // SAFETY: `path_ptr` points into `path_cstring`, which is NUL-terminated and
    // stays alive for this call.
    let url = unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
            path_ptr,
            file_type.is_dir(),
            None,
        )
    };

    let mut trashed_url = None;

    file_manager
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut trashed_url))
        .map_err(|error| Error::Platform {
            message: format!("File manager rejected {}: {error}", path.display()),
        })?;

    let trashed_url = trashed_url.ok_or_else(|| Error::Platform {
        message: format!(
            "File manager did not return a trashed item URL for {}",
            path.display()
        ),
    })?;
    let original_name = path
        .file_name()
        .ok_or_else(|| Error::TargetedRoot {
            path: path.to_path_buf(),
        })?
        .to_os_string();
    let original_parent = path
        .parent()
        .ok_or_else(|| Error::TargetedRoot {
            path: path.to_path_buf(),
        })?
        .to_path_buf();

    // SAFETY: `trashed_url` comes from `trashItemAtURL`; its filesystem path is
    // NUL-terminated and copied before `trashed_url` is dropped.
    let trashed_path = unsafe { CStr::from_ptr(trashed_url.fileSystemRepresentation().as_ptr()) };
    let trashed_id = std::ffi::OsString::from_vec(trashed_path.to_bytes().to_vec());

    Ok(TrashItem::new(
        trashed_id,
        original_name,
        original_parent,
        SystemTime::now(),
    ))
}
