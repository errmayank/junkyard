mod location;
mod mount;
mod payload;
mod permission;
mod trash_info;

use std::{
    collections::HashSet,
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};
use time::OffsetDateTime;

use location::{TrashDirectory, TrashLocation};
use payload::PayloadKind;
use trash_info::ReservedTrashEntry;

use crate::{Error, Result, Trash, TrashItem};

const OWNER_RWX_MODE: u32 = 0o700;
const STAT_BLOCK_SIZE: u64 = 512;

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
    let discarded_at = current_local_time()?;

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
            let result = update_directory_size_cache(&trash_dir, &entry);

            // The item is already in trash, so cache update failures are intentionally non-fatal.
            drop(result);
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

fn current_local_time() -> Result<OffsetDateTime> {
    OffsetDateTime::now_local().map_err(|source| Error::Platform {
        message: format!("Failed to get local time: {source}"),
    })
}

fn update_directory_size_cache(
    trash_dir: &TrashDirectory,
    entry: &ReservedTrashEntry,
) -> Result<()> {
    let cache_path = trash_dir.path.join("directorysizes");
    let size = directory_disk_usage(&entry.file)?;
    let mtime = entry
        .info
        .metadata()
        .map_err(|source| Error::Io {
            path: entry.info.clone(),
            source,
        })?
        .mtime();
    let name = trash_info::percent_encode_path(Path::new(entry.name.as_os_str()));
    let contents = directory_size_cache_contents(&cache_path, size, mtime, &name)?;

    write_directory_size_cache(&cache_path, &contents)
}

fn directory_size_cache_contents(
    cache_path: &Path,
    size: u64,
    mtime: i64,
    name: &str,
) -> Result<String> {
    let mut contents = String::new();

    match std::fs::read_to_string(cache_path) {
        Ok(existing_contents) => {
            for line in existing_contents.lines() {
                if line.split_ascii_whitespace().nth(2) == Some(name) {
                    continue;
                }

                contents.push_str(line);
                contents.push('\n');
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                path: cache_path.to_owned(),
                source,
            });
        }
    }

    fmt::Write::write_fmt(&mut contents, format_args!("{size} {mtime} {name}\n")).map_err(
        |source| Error::Platform {
            message: format!("Failed to append directory size cache entry: {source}"),
        },
    )?;

    Ok(contents)
}

fn write_directory_size_cache(cache_path: &Path, contents: &str) -> Result<()> {
    let parent = cache_path.parent().ok_or_else(|| Error::Platform {
        message: format!(
            "Directory size cache path has no parent: {}",
            cache_path.display()
        ),
    })?;
    let process_id = std::process::id();
    let mut collision_index = 0usize;

    loop {
        let temporary_file = parent.join(format!(
            ".directorysizes.{process_id}.{collision_index}.tmp"
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_file)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                collision_index =
                    collision_index
                        .checked_add(1)
                        .ok_or_else(|| Error::Platform {
                            message: format!(
                                "Could not find available trash name for {}",
                                cache_path.display()
                            ),
                        })?;

                continue;
            }
            Err(source) => {
                return Err(Error::Io {
                    path: temporary_file,
                    source,
                });
            }
        };

        if let Err(source) = file.write_all(contents.as_bytes()) {
            let write_path = temporary_file.clone();
            payload::remove_file_if_exists(&temporary_file)?;

            return Err(Error::Io {
                path: write_path,
                source,
            });
        }

        drop(file);

        if let Err(source) = std::fs::rename(&temporary_file, cache_path) {
            let rename_path = cache_path.to_owned();
            payload::remove_file_if_exists(&temporary_file)?;

            return Err(Error::Io {
                path: rename_path,
                source,
            });
        }

        return Ok(());
    }
}

fn directory_disk_usage(path: &Path) -> Result<u64> {
    fn inner(path: &Path, visited: &mut HashSet<(u64, u64)>) -> Result<u64> {
        let metadata = path.symlink_metadata().map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;

        if !visited.insert((metadata.dev(), metadata.ino())) {
            return Ok(0);
        }

        let mut size = metadata
            .blocks()
            .checked_mul(STAT_BLOCK_SIZE)
            .ok_or_else(|| Error::Platform {
                message: format!("Directory size overflow for {}", path.display()),
            })?;

        if !metadata.file_type().is_dir() {
            return Ok(size);
        }

        for entry in std::fs::read_dir(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            let entry_path = entry.path();
            let entry_size = inner(&entry_path, visited)?;

            size = size
                .checked_add(entry_size)
                .ok_or_else(|| Error::Platform {
                    message: format!("Directory size overflow for {}", path.display()),
                })?;
        }

        Ok(size)
    }

    inner(path, &mut HashSet::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;
    use std::{
        ffi::OsStr,
        os::unix::{self, fs::MetadataExt},
    };
    use tempfile::TempDir;

    use location::HomeTrashPath;
    use mount::MountPoint;

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
        let trashed_dir = trash.join("files/Camera");
        let info_mtime = Path::new(trashed_item.id()).metadata().unwrap().mtime();

        assert_eq!(size, directory_disk_usage(&trashed_dir).unwrap());
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

    #[test]
    fn test_directory_size_cache_contents_replaces_existing_entry() {
        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("directorysizes");

        std::fs::write(
            &cache_path,
            indoc! {"
                2048 1 Other
                1024 2 Camera
            "},
        )
        .unwrap();

        let contents =
            directory_size_cache_contents(&cache_path, 4096, 1_779_555_000, "Camera").unwrap();

        assert_eq!(
            contents,
            indoc! {"
                2048 1 Other
                4096 1779555000 Camera
            "}
        );
    }

    #[test]
    fn test_directory_disk_usage_does_not_follow_directory_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path().join("project");
        let shared_dir = temp_dir.path().join("shared");
        let shared_link = project_dir.join("shared-link");

        std::fs::create_dir(&project_dir).unwrap();
        std::fs::create_dir(&shared_dir).unwrap();
        std::fs::write(shared_dir.join("LICENSE"), b"license text").unwrap();
        unix::fs::symlink(&shared_dir, &shared_link).unwrap();

        let size = directory_disk_usage(&project_dir).unwrap();
        let expected_size = (project_dir.symlink_metadata().unwrap().blocks()
            + shared_link.symlink_metadata().unwrap().blocks())
            * STAT_BLOCK_SIZE;

        assert_eq!(size, expected_size);
    }
}
