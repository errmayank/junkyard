use std::{fs::OpenOptions, io, path::Path};

use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PayloadKind {
    File,
    Directory,
}

pub(super) fn move_to_trash(path: &Path, trash_path: &Path) -> Result<PayloadKind> {
    let file_type = path
        .symlink_metadata()
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .file_type();
    let payload_kind = if file_type.is_dir() {
        PayloadKind::Directory
    } else {
        PayloadKind::File
    };

    create_trash_placeholder(trash_path, payload_kind)?;

    match std::fs::rename(path, trash_path) {
        Ok(()) => Ok(payload_kind),
        Err(source) => {
            remove_trash_placeholder(trash_path, payload_kind)?;

            Err(Error::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

pub(super) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn create_trash_placeholder(path: &Path, payload_kind: PayloadKind) -> Result<()> {
    match payload_kind {
        PayloadKind::File => OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop)
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            }),
        PayloadKind::Directory => std::fs::create_dir(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_trash_placeholder(path: &Path, payload_kind: PayloadKind) -> Result<()> {
    let result = match payload_kind {
        PayloadKind::File => std::fs::remove_file(path),
        PayloadKind::Directory => std::fs::remove_dir(path),
    };

    match result {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}
