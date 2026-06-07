use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct DiscardTarget {
    pub(crate) path: PathBuf,

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) original_name: OsString,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) original_parent: PathBuf,
}

impl DiscardTarget {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn new(path: PathBuf, original_name: OsString, original_parent: PathBuf) -> Self {
        Self {
            path,
            original_name,
            original_parent,
        }
    }

    #[cfg(target_os = "windows")]
    fn new(path: PathBuf, _: OsString, _: PathBuf) -> Self {
        Self { path }
    }
}

pub(crate) fn resolve_target(path: &Path) -> Result<DiscardTarget> {
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

        let original_name = path
            .file_name()
            .ok_or_else(|| Error::TargetedRoot { path: path.clone() })?
            .to_os_string();
        let original_parent = path
            .parent()
            .ok_or_else(|| Error::TargetedRoot { path: path.clone() })?
            .to_path_buf();

        return Ok(DiscardTarget::new(path, original_name, original_parent));
    };

    let parent = path
        .parent()
        .ok_or_else(|| Error::TargetedRoot { path: path.clone() })
        .and_then(|parent| {
            parent.canonicalize().map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })
        })?;

    Ok(DiscardTarget::new(
        parent.join(file_name),
        file_name.to_os_string(),
        parent,
    ))
}
