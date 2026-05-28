use std::{
    env,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::EmptyPath);
    }

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };

    let Some(file_name) = path.file_name() else {
        let path = path.canonicalize().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;

        if path.parent().is_none() {
            return Err(Error::TargetedRoot { path });
        }

        return Ok(path);
    };

    let parent = path
        .parent()
        .ok_or_else(|| Error::TargetedRoot { path: path.clone() })?;

    let parent = parent.canonicalize().map_err(|source| Error::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    Ok(parent.join(file_name))
}
