use indoc::formatdoc;
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;

use super::location::{TrashDirectory, TrashLocation};
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReservedTrashEntry {
    pub(super) name: OsString,
    pub(super) file: PathBuf,
    pub(super) info: PathBuf,
}

pub(super) fn reserve_entry(
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
            collision_index = collision_index
                .checked_add(1)
                .ok_or_else(|| Error::Platform {
                    message: format!(
                        "Could not find available trash name for {}",
                        original_path.display()
                    ),
                })?;
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
                collision_index =
                    collision_index
                        .checked_add(1)
                        .ok_or_else(|| Error::Platform {
                            message: format!(
                                "Could not find available trash name for {}",
                                original_path.display()
                            ),
                        })?;

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

pub(super) fn percent_encode_path(path: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;
    use std::{
        ffi::OsString,
        os::unix::ffi::OsStringExt,
        path::{Path, PathBuf},
    };
    use tempfile::TempDir;

    use crate::platform::linux::{
        location::{ExternalTrashPath, HomeTrashPath},
        mount::MountPoint,
    };

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
        let first = reserve_entry(&location, &trash_dir, original_path, discarded_at).unwrap();
        let second = reserve_entry(&location, &trash_dir, original_path, discarded_at).unwrap();

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
        let item = reserve_entry(&location, &trash_dir, original_path, discarded_at).unwrap();

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
        let item = reserve_entry(&location, &trash_dir, original_path, discarded_at).unwrap();

        assert_eq!(
            std::fs::read_to_string(&item.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=Downloads/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }
}
