mod directory_size;
mod location;
mod mount;
mod payload;
mod permission;
mod trash_info;

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};
use time::OffsetDateTime;

use directory_size::DirectorySizeCache;
use location::TrashLocation;
use payload::PayloadKind;

use crate::{Error, Result, Trash, TrashItem};

pub(crate) fn discard(_: &Trash, path: &Path) -> Result<TrashItem> {
    let location = TrashLocation::resolve(path)?;

    discard_inner(&location, path)
}

pub(crate) fn discard_all(trash: &Trash, paths: &[PathBuf]) -> Result<Vec<TrashItem>> {
    paths.iter().map(|path| discard(trash, path)).collect()
}

fn discard_inner(location: &TrashLocation, path: &Path) -> Result<TrashItem> {
    permission::ensure_discard_permission(path)?;

    let trash_dir = location.prepare()?;
    let discarded_at = OffsetDateTime::now_local().map_err(|source| Error::Platform {
        message: format!("Failed to get local time: {source}"),
    })?;

    loop {
        let entry = trash_info::reserve_entry(location, &trash_dir, path, discarded_at)?;
        let moved_payload = match payload::move_to_trash(path, &entry.file) {
            Ok(moved_payload) => moved_payload,
            Err(error) => {
                payload::remove_file_if_exists(&entry.info)?;

                if matches!(
                    &error,
                    Error::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists
                ) {
                    continue;
                }

                return Err(error);
            }
        };

        if moved_payload == PayloadKind::Directory {
            let directory_size_cache = DirectorySizeCache::new(&trash_dir);

            if let Err(_source) = directory_size_cache.update(&entry) {
                // The item is already in trash, so cache update failures are intentionally non-fatal.
            }
        }

        let original_parent = path
            .parent()
            .ok_or_else(|| Error::TargetedRoot {
                path: path.to_path_buf(),
            })?
            .to_path_buf();
        let original_name = path
            .file_name()
            .ok_or_else(|| Error::TargetedRoot {
                path: path.to_path_buf(),
            })?
            .to_os_string();

        return Ok(TrashItem::new(
            entry.info.into_os_string(),
            original_name,
            original_parent,
            SystemTime::from(discarded_at),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{ffi::OsStr, os::unix::fs::MetadataExt};
    use tempfile::TempDir;

    use location::HomeTrashPath;
    use mount::MountPoint;
    use trash_info::ReservedTrashEntry;

    #[test]
    fn test_discard_inner_updates_directory_size_cache() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let location = TrashLocation::Home {
            path: HomeTrashPath(trash.clone()),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };
        let dir = temp_dir.path().join("Downloads/Camera");
        let file = dir.join("DSC_00001.jpg");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"image bytes").unwrap();

        let trashed_item = discard_inner(&location, &dir).unwrap();
        let contents = std::fs::read_to_string(trash.join("directorysizes")).unwrap();
        let mut fields = contents.split_ascii_whitespace();
        let size = fields.next().unwrap().parse::<u64>().unwrap();
        let mtime = fields.next().unwrap().parse::<i64>().unwrap();
        let name = fields.next().unwrap();
        let info_mtime = Path::new(trashed_item.id()).metadata().unwrap().mtime();
        let trash_dir = location.prepare().unwrap();
        let directory_size_cache = DirectorySizeCache::new(&trash_dir);
        let entry = ReservedTrashEntry {
            name: "Camera".into(),
            file: trash_dir.files.join("Camera"),
            info: Path::new(trashed_item.id()).to_path_buf(),
        };

        assert_eq!(size, directory_size_cache.disk_usage(&entry).unwrap());
        assert_eq!(mtime, info_mtime);
        assert_eq!(name, "Camera");
        assert_eq!(fields.next(), None);
    }

    #[test]
    fn test_discard_inner_succeeds_if_directory_size_cache_fails() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let location = TrashLocation::Home {
            path: HomeTrashPath(trash.clone()),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };
        let dir = temp_dir.path().join("Downloads/Camera");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(trash.join("directorysizes")).unwrap();

        let trashed_item = discard_inner(&location, &dir).unwrap();

        assert_eq!(trashed_item.original_name(), OsStr::new("Camera"));
        assert!(!dir.exists());
        assert!(trash.join("directorysizes").is_dir());
    }
}
