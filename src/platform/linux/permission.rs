use rustix::{
    fs::{Access, AtFlags, CWD},
    process,
};
use std::{
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

use crate::{Error, Result};

pub(super) const STICKY_BIT: u32 = 0o1000;
#[cfg(test)]
pub(super) const OWNER_RX_MODE: u32 = 0o500;
#[cfg(test)]
pub(super) const OWNER_RWX_MODE: u32 = 0o700;
#[cfg(test)]
pub(super) const OWNER_RWX_WORLD_RX_MODE: u32 = 0o755;
#[cfg(test)]
pub(super) const WORLD_RWX_MODE: u32 = 0o0777;
#[cfg(test)]
pub(super) const WORLD_RWX_STICKY_MODE: u32 = WORLD_RWX_MODE | STICKY_BIT;
#[cfg(test)]
pub(super) const PERMISSION_BITS_MASK: u32 = 0o777;

pub(super) fn ensure_discard_permission(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::TargetedRoot {
        path: path.to_path_buf(),
    })?;
    let metadata = path.symlink_metadata().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parent_metadata = parent.symlink_metadata().map_err(|source| Error::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    rustix::fs::accessat(
        CWD,
        parent,
        Access::WRITE_OK | Access::EXEC_OK,
        AtFlags::EACCESS,
    )
    .map_err(|source| Error::Io {
        path: parent.to_path_buf(),
        source: io::Error::from(source),
    })?;

    if parent_metadata.mode() & STICKY_BIT == 0 {
        return Ok(());
    }

    let user_id = process::geteuid().as_raw();
    let is_root = user_id == 0;
    let is_path_owner = user_id == metadata.uid();
    let is_parent_owner = user_id == parent_metadata.uid();

    if is_root || is_path_owner || is_parent_owner {
        return Ok(());
    }

    Err(Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sticky parent directory prevents deleting path",
        ),
    })
}

pub(super) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = path
        .metadata()
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .permissions();

    permissions.set_mode(mode);

    std::fs::set_permissions(path, permissions).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustix::process;
    use std::{io, os::unix::fs::MetadataExt};
    use tempfile::TempDir;

    #[test]
    fn test_ensure_discard_permission() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("directory");
        let file = dir.join("file.txt");

        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&file, b"contents").unwrap();

        ensure_discard_permission(&file).expect("parent directory should allow discard");

        assert!(
            process::geteuid().as_raw() != 0,
            "unwritable parent rejection requires non-root user"
        );

        set_mode(&dir, OWNER_RX_MODE).unwrap();
        assert_eq!(
            dir.metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RX_MODE
        );
        let error = ensure_discard_permission(&file).unwrap_err();

        assert!(matches!(
            error,
            Error::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied
        ));

        set_mode(&dir, OWNER_RWX_MODE).unwrap();
        assert_eq!(
            dir.metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
        ensure_discard_permission(&file).expect("parent directory should allow discard");
    }

    #[test]
    fn test_ensure_discard_permission_with_sticky_parent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("directory");
        let file = dir.join("file.txt");

        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&file, b"contents").unwrap();
        set_mode(&dir, WORLD_RWX_STICKY_MODE).unwrap();

        ensure_discard_permission(&file).expect("path owner should be allowed in sticky parent");
    }
}
