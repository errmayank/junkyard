mod location;
mod mount;
mod payload;
mod permission;

use indoc::formatdoc;
use std::{
    collections::HashSet,
    ffi::OsString,
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::SystemTime,
};
use time::OffsetDateTime;

use location::{TrashDirectory, TrashLocation};
use payload::PayloadKind;

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
        let entry = create_trash_info(location, &trash_dir, path, discarded_at)?;
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

fn path_exists(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReservedTrashEntry {
    name: OsString,
    file: PathBuf,
    info: PathBuf,
}

fn next_collision_index(collision_index: usize, path: &Path) -> Result<usize> {
    collision_index
        .checked_add(1)
        .ok_or_else(|| Error::Platform {
            message: format!("Could not find available trash name for {}", path.display()),
        })
}

fn create_trash_info(
    location: &TrashLocation,
    trash_dir: &TrashDirectory,
    original_path: &Path,
    discarded_at: OffsetDateTime,
) -> Result<ReservedTrashEntry> {
    let original_name = original_path
        .file_name()
        .ok_or_else(|| Error::TargetedRoot {
            path: original_path.to_path_buf(),
        })?;
    let original_location = location.trash_info_path(original_path)?;
    let mut collision_index = 0usize;

    loop {
        let name = {
            let mut name = original_name.to_os_string();
            if collision_index != 0 {
                name.push(format!(".{collision_index}"));
            }

            name
        };
        let file = trash_dir.files.join(&name);

        if path_exists(&file)? {
            collision_index = next_collision_index(collision_index, original_path)?;
            continue;
        }

        let info = {
            let mut info_name = name.clone();
            info_name.push(".trashinfo");

            trash_dir.info.join(info_name)
        };

        let mut info_file = match OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(info_file) => info_file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                collision_index = next_collision_index(collision_index, original_path)?;

                continue;
            }
            Err(source) => {
                return Err(Error::Io { path: info, source });
            }
        };

        let contents = trash_info_contents(&original_location, discarded_at);

        if let Err(source) = info_file.write_all(contents.as_bytes()) {
            let write_path = info.clone();

            match std::fs::remove_file(&info) {
                Ok(()) => {}
                Err(cleanup_source) if cleanup_source.kind() == io::ErrorKind::NotFound => {}
                Err(cleanup_source) => {
                    return Err(Error::Io {
                        path: info,
                        source: cleanup_source,
                    });
                }
            }

            return Err(Error::Io {
                path: write_path,
                source,
            });
        }

        return Ok(ReservedTrashEntry { name, file, info });
    }
}

fn current_local_time() -> Result<OffsetDateTime> {
    OffsetDateTime::now_local().map_err(|source| Error::Platform {
        message: format!("Failed to get local time: {source}"),
    })
}

fn trash_info_contents(original_location: &Path, discarded_at: OffsetDateTime) -> String {
    let path = percent_encode_path(original_location);
    let deletion_date = {
        let year = discarded_at.year();
        let month = u8::from(discarded_at.month());
        let day = discarded_at.day();
        let hour = discarded_at.hour();
        let minute = discarded_at.minute();
        let second = discarded_at.second();

        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
    };

    formatdoc! {"
        [Trash Info]
        Path={path}
        DeletionDate={deletion_date}
    "}
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
    let name = percent_encode_path(Path::new(entry.name.as_os_str()));
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
                collision_index = next_collision_index(collision_index, cache_path)?;

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

fn percent_encode_path(path: &Path) -> String {
    let mut encoded = String::new();

    for byte in path.as_os_str().as_bytes() {
        if *byte == b'/' || is_unreserved_url_byte(*byte) {
            encoded.push(char::from(*byte));
        } else {
            push_percent_encoded_byte(&mut encoded, *byte);
        }
    }

    encoded
}

fn is_unreserved_url_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
    )
}

fn push_percent_encoded_byte(output: &mut String, byte: u8) {
    let push_hex_digit = |output: &mut String, value: u8| {
        let digit = match value {
            0..=9 => char::from(b'0' + value),
            10..=15 => char::from(b'A' + (value - 10)),
            _ => return,
        };

        output.push(digit);
    };

    output.push('%');
    push_hex_digit(output, byte >> 4);
    push_hex_digit(output, byte & 0x0f);
}

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;
    use std::{
        ffi::OsStr,
        os::unix::{self, ffi::OsStringExt, fs::MetadataExt},
    };
    use tempfile::TempDir;

    use location::{ExternalTrashPath, HomeTrashPath};
    use mount::MountPoint;

    #[test]
    fn test_percent_encode_path() {
        let path = PathBuf::from(OsString::from_vec(
            b"/home/user/Downloads/CPU usage %.log".to_vec(),
        ));

        assert_eq!(
            percent_encode_path(&path),
            "/home/user/Downloads/CPU%20usage%20%25.log"
        );
    }

    #[test]
    fn test_percent_encode_path_preserves_invalid_utf8() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/invalid-\xff.txt".to_vec()));

        assert_eq!(percent_encode_path(&path), "/tmp/invalid-%FF.txt");
    }

    #[test]
    fn test_trash_info_contents() {
        let discarded_at = OffsetDateTime::from_unix_timestamp(1_779_555_000).unwrap();
        let original_path = Path::new("/home/user/Downloads/clip 01.mp4");

        let contents = trash_info_contents(original_path, discarded_at);

        assert_eq!(
            contents,
            indoc! {"
                [Trash Info]
                Path=/home/user/Downloads/clip%2001.mp4
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }

    #[test]
    fn test_create_trash_info_with_duplicate_trashinfo() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let trash_dir = TrashDirectory::prepare(&trash).unwrap();
        let location = TrashLocation::Home {
            path: HomeTrashPath(trash.clone()),
            mount_point: MountPoint(PathBuf::from("/home")),
        };
        let original_path = Path::new("/home/user/Downloads/file.txt");

        let discarded_at = OffsetDateTime::from_unix_timestamp(1_779_555_000).unwrap();
        let first = create_trash_info(&location, &trash_dir, original_path, discarded_at).unwrap();
        let second = create_trash_info(&location, &trash_dir, original_path, discarded_at).unwrap();

        assert_eq!(
            first,
            ReservedTrashEntry {
                name: OsString::from("file.txt"),
                file: trash_dir.files.join("file.txt"),
                info: trash_dir.info.join("file.txt.trashinfo"),
            }
        );
        assert_eq!(
            second,
            ReservedTrashEntry {
                name: OsString::from("file.txt.1"),
                file: trash_dir.files.join("file.txt.1"),
                info: trash_dir.info.join("file.txt.1.trashinfo"),
            }
        );
        assert!(!first.file.exists());
        assert!(!second.file.exists());
        assert_eq!(
            std::fs::read_to_string(&first.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=/home/user/Downloads/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
        assert_eq!(
            std::fs::read_to_string(&second.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=/home/user/Downloads/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }

    #[test]
    fn test_create_trash_info_with_duplicate_payload() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let trash_dir = TrashDirectory::prepare(&trash).unwrap();
        let location = TrashLocation::Home {
            path: HomeTrashPath(trash.clone()),
            mount_point: MountPoint(PathBuf::from("/home")),
        };
        let original_path = Path::new("/home/user/Downloads/file.txt");

        std::fs::write(trash_dir.files.join("file.txt"), b"existing").unwrap();

        let discarded_at = OffsetDateTime::from_unix_timestamp(1_779_555_000).unwrap();
        let item = create_trash_info(&location, &trash_dir, original_path, discarded_at).unwrap();

        assert_eq!(
            item,
            ReservedTrashEntry {
                name: OsString::from("file.txt.1"),
                file: trash_dir.files.join("file.txt.1"),
                info: trash_dir.info.join("file.txt.1.trashinfo"),
            }
        );
        assert!(!item.file.exists());
        assert_eq!(
            std::fs::read_to_string(&item.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=/home/user/Downloads/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }

    #[test]
    fn test_create_trash_info_with_external_location() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let trash_dir = TrashDirectory::prepare(&trash).unwrap();
        let location = TrashLocation::External {
            path: ExternalTrashPath {
                path: trash.clone(),
                fallback_path: None,
            },
            mount_point: MountPoint(PathBuf::from("/media/usb")),
        };
        let original_path = Path::new("/media/usb/Downloads/file.txt");

        let discarded_at = OffsetDateTime::from_unix_timestamp(1_779_555_000).unwrap();
        let item = create_trash_info(&location, &trash_dir, original_path, discarded_at).unwrap();

        assert_eq!(
            std::fs::read_to_string(&item.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=Downloads/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }

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
