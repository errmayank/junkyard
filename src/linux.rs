use rustix::process;
use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

use crate::{Error, Result, Trash, TrashItem};

const MOUNT_INFO_PATH: &str = "/proc/self/mountinfo";

pub(crate) fn discard(_: &Trash, path: &Path) -> Result<TrashItem> {
    unimplemented!()
}

pub(crate) fn discard_all(_: &Trash, paths: &[PathBuf]) -> Result<Vec<TrashItem>> {
    unimplemented!()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mounts(Vec<MountInfo>);

impl Mounts {
    fn read() -> Result<Self> {
        let contents = std::fs::read(MOUNT_INFO_PATH).map_err(|source| Error::Io {
            path: PathBuf::from(MOUNT_INFO_PATH),
            source,
        })?;
        let entries = parse_mount_info(&contents).map_err(|source| Error::Platform {
            message: format!("Failed to parse {MOUNT_INFO_PATH}: {source:?}"),
        })?;

        if entries.is_empty() {
            return Err(Error::Platform {
                message: format!("No mount points found in {MOUNT_INFO_PATH}"),
            });
        }

        Ok(Self::new(entries))
    }

    fn new(mut entries: Vec<MountInfo>) -> Self {
        entries.sort_unstable_by(|left, right| {
            let left_length = left.mount_point.as_path().as_os_str().as_bytes().len();
            let right_length = right.mount_point.as_path().as_os_str().as_bytes().len();

            right_length.cmp(&left_length)
        });

        Self(entries)
    }

    fn find_mount_point(&self, path: &Path) -> Option<MountPoint> {
        self.0
            .iter()
            .find(|info| path.starts_with(info.mount_point.as_path()))
            .map(|info| info.mount_point.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MountPoint(PathBuf);

impl MountPoint {
    fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MountInfo {
    mount_point: MountPoint,
}

#[derive(Debug, Eq, PartialEq)]
enum MountParseError {
    MissingSeparator,
    MissingMountPoint,
    InvalidPathEscape,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrashDirectory {
    path: PathBuf,
    files: PathBuf,
    info: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExternalTrashPath {
    path: PathBuf,
    fallback_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TrashLocation {
    Home {
        path: PathBuf,
        mount_point: MountPoint,
    },
    External {
        path: ExternalTrashPath,
        mount_point: MountPoint,
    },
}

impl TrashLocation {
    fn resolve(path: &Path) -> Result<Self> {
        let mounts = Mounts::read()?;
        let user_id = process::getuid().as_raw();
        let xdg_data_home = env::var_os("XDG_DATA_HOME");
        let home = env::var_os("HOME");
        let home_trash = home_trash_path(xdg_data_home.as_deref(), home.as_deref())?;
        let home_trash = canonicalize_nearest_existing_parent(&home_trash)?;

        Self::select(path, &mounts, &home_trash, user_id)
    }

    fn select(path: &Path, mounts: &Mounts, home_trash: &Path, user_id: u32) -> Result<Self> {
        let target_mount = mounts
            .find_mount_point(path)
            .ok_or_else(|| Error::Platform {
                message: format!("No mount point found for {}", path.display()),
            })?;
        let home_mount = mounts
            .find_mount_point(home_trash)
            .ok_or_else(|| Error::Platform {
                message: format!("No mount point found for {}", home_trash.display()),
            })?;

        if target_mount == home_mount {
            return Ok(Self::Home {
                path: home_trash.to_path_buf(),
                mount_point: target_mount,
            });
        }

        let external_trash = external_trash_path(target_mount.as_path(), user_id);

        Ok(Self::External {
            path: external_trash,
            mount_point: target_mount,
        })
    }

    fn prepare(&self) -> Result<TrashDirectory> {
        match self {
            Self::Home { path, .. } => prepare_trash_directory(path),
            Self::External { path, .. } => prepare_external_trash_directory(path),
        }
    }
}

fn home_trash_path(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(xdg_data_home) = xdg_data_home
        && !xdg_data_home.is_empty()
    {
        let xdg_data_home = Path::new(xdg_data_home);

        if xdg_data_home.is_absolute() {
            return Ok(xdg_data_home.join("Trash"));
        }
    }

    if let Some(home) = home
        && !home.is_empty()
    {
        let home = Path::new(home);

        if home.is_absolute() {
            return Ok(home.join(".local/share/Trash"));
        }
    }

    Err(Error::Platform {
        message: "No absolute XDG_DATA_HOME or HOME is available".to_owned(),
    })
}

fn external_trash_path(top_dir: &Path, user_id: u32) -> ExternalTrashPath {
    let fallback_trash = top_dir.join(format!(".Trash-{user_id}"));
    let shared_trash = top_dir.join(".Trash");

    if let Ok(metadata) = shared_trash.symlink_metadata() {
        let file_type = metadata.file_type();
        let has_sticky_bit = metadata.mode() & 0o1000 != 0;

        if file_type.is_dir() && !file_type.is_symlink() && has_sticky_bit {
            return ExternalTrashPath {
                path: shared_trash.join(user_id.to_string()),
                fallback_path: Some(fallback_trash),
            };
        }
    }

    ExternalTrashPath {
        path: fallback_trash,
        fallback_path: None,
    }
}

fn prepare_trash_directory(path: &Path) -> Result<TrashDirectory> {
    let directory = TrashDirectory {
        path: path.to_owned(),
        files: path.join("files"),
        info: path.join("info"),
    };

    for path in [
        directory.path.as_path(),
        directory.files.as_path(),
        directory.info.as_path(),
    ] {
        std::fs::create_dir_all(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
    }

    Ok(directory)
}

fn prepare_trash_directory_without_creating_parent(path: &Path) -> Result<TrashDirectory> {
    let shared_trash = path.parent().ok_or_else(|| Error::Platform {
        message: format!("Shared trash path has no parent: {}", path.display()),
    })?;
    let metadata = shared_trash
        .symlink_metadata()
        .map_err(|source| Error::Io {
            path: shared_trash.to_owned(),
            source,
        })?;
    let file_type = metadata.file_type();
    let has_sticky_bit = metadata.mode() & 0o1000 != 0;

    if file_type.is_symlink() {
        return Err(Error::Platform {
            message: format!("Shared trash path is a symlink: {}", shared_trash.display()),
        });
    }

    if !file_type.is_dir() {
        return Err(Error::Platform {
            message: format!(
                "Shared trash path is not a directory: {}",
                shared_trash.display()
            ),
        });
    }

    if !has_sticky_bit {
        return Err(Error::Platform {
            message: format!(
                "Shared trash directory is missing sticky bit: {}",
                shared_trash.display()
            ),
        });
    }

    let directory = TrashDirectory {
        path: path.to_owned(),
        files: path.join("files"),
        info: path.join("info"),
    };

    for path in [
        directory.path.as_path(),
        directory.files.as_path(),
        directory.info.as_path(),
    ] {
        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = path.symlink_metadata().map_err(|source| Error::Io {
                    path: path.to_owned(),
                    source,
                })?;
                let file_type = metadata.file_type();

                if file_type.is_symlink() {
                    return Err(Error::Io {
                        path: path.to_owned(),
                        source: io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "path exists but is a symlink",
                        ),
                    });
                }

                if !file_type.is_dir() {
                    return Err(Error::Io {
                        path: path.to_owned(),
                        source: io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "path exists but is not a directory",
                        ),
                    });
                }
            }
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }

    Ok(directory)
}

fn prepare_external_trash_directory(path: &ExternalTrashPath) -> Result<TrashDirectory> {
    match &path.fallback_path {
        Some(fallback) => prepare_trash_directory_without_creating_parent(&path.path)
            .or_else(|_| prepare_trash_directory(fallback)),
        None => prepare_trash_directory(&path.path),
    }
}

fn canonicalize_nearest_existing_parent(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(mut canonical) => {
                let suffix = path
                    .strip_prefix(ancestor)
                    .map_err(|source| Error::Platform {
                        message: format!("Failed to resolve {}: {source}", path.display()),
                    })?;

                canonical.push(suffix);
                return Ok(canonical);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    path: ancestor.to_path_buf(),
                    source,
                });
            }
        }
    }

    Err(Error::Platform {
        message: format!("Path has no existing parent: {}", path.display()),
    })
}

fn parse_mount_info(contents: &[u8]) -> std::result::Result<Vec<MountInfo>, MountParseError> {
    let parse_line = |line: &[u8]| {
        let mut field_number = 0;
        let mut mount_point = None;
        let mut found_separator = false;

        for field in line.split(u8::is_ascii_whitespace) {
            if field.is_empty() {
                continue;
            }

            if field == b"-" {
                found_separator = true;
                break;
            }

            field_number += 1;

            if field_number == 5 {
                mount_point = Some(field);
            }
        }

        if !found_separator {
            return Err(MountParseError::MissingSeparator);
        }

        let mount_point = mount_point.ok_or(MountParseError::MissingMountPoint)?;
        let mount_point =
            decode_mount_info_path(mount_point).ok_or(MountParseError::InvalidPathEscape)?;

        Ok(MountInfo {
            mount_point: MountPoint(mount_point),
        })
    };

    let mut entries = Vec::new();

    for line in contents.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }

        entries.push(parse_line(line)?);
    }

    Ok(entries)
}

fn decode_mount_info_path(path: &[u8]) -> Option<PathBuf> {
    let decode_octal_escape = |first: u8, second: u8, third: u8| {
        let octal_digit = |byte| match byte {
            b'0'..=b'7' => Some(byte - b'0'),
            _ => None,
        };

        let first = u16::from(octal_digit(first)?);
        let second = u16::from(octal_digit(second)?);
        let third = u16::from(octal_digit(third)?);
        let value = (first * 64) + (second * 8) + third;

        u8::try_from(value).ok()
    };

    let mut bytes = path.iter().copied();
    let mut decoded = Vec::with_capacity(path.len());

    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }

        let first = bytes.next()?;
        let second = bytes.next()?;
        let third = bytes.next()?;

        decoded.push(decode_octal_escape(first, second, third)?);
    }

    Some(PathBuf::from(OsString::from_vec(decoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::{self, fs::PermissionsExt};
    use tempfile::TempDir;

    fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);

        std::fs::set_permissions(path, permissions)
    }

    #[test]
    fn test_parse_mount_info() {
        let bytes = concat!(
            "36 35 98:0 / / rw,relatime - ext4 /dev/root rw\n",
            "37 36 8:1 / /home rw,nosuid - ext4 /dev/sda1 rw\n",
        )
        .as_bytes();
        let entries = parse_mount_info(bytes).unwrap();

        assert_eq!(
            entries,
            vec![
                MountInfo {
                    mount_point: MountPoint(PathBuf::from("/")),
                },
                MountInfo {
                    mount_point: MountPoint(PathBuf::from("/home")),
                },
            ]
        );
    }

    #[test]
    fn test_parse_mount_info_decodes_path_escapes() {
        let bytes = b"37 36 8:1 / /media/My\\040Drive/Slash\\134Name rw - ext4 /dev/sda1 rw";
        let entries = parse_mount_info(bytes).unwrap();

        assert_eq!(
            entries,
            vec![MountInfo {
                mount_point: MountPoint(PathBuf::from("/media/My Drive/Slash\\Name")),
            }]
        );
    }

    #[test]
    fn test_parse_mount_info_preserves_invalid_utf8() {
        let bytes = b"37 36 8:1 / /media/invalid\\377path rw - ext4 /dev/sda1 rw";
        let entries = parse_mount_info(bytes).unwrap();

        assert_eq!(
            entries,
            vec![MountInfo {
                mount_point: MountPoint(PathBuf::from(OsString::from_vec(
                    b"/media/invalid\xffpath".to_vec()
                ))),
            }]
        );
    }

    #[test]
    fn test_parse_mount_info_rejects_invalid_path_escape() {
        let bytes = b"37 36 8:1 / /media/invalid\\999path rw - ext4 /dev/sda1 rw";
        let error = parse_mount_info(bytes).unwrap_err();

        assert_eq!(error, MountParseError::InvalidPathEscape);
    }

    #[test]
    fn test_find_mount_point_uses_longest_match() {
        let mounts = Mounts::new(vec![
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/foo")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/foo/bar")),
            },
        ]);

        let mount_point = mounts
            .find_mount_point(Path::new("/foo/bar/baz.txt"))
            .unwrap();

        assert_eq!(mount_point.as_path(), Path::new("/foo/bar"));
    }

    #[test]
    fn test_find_mount_point_does_not_match_partial_component() {
        let mounts = Mounts::new(vec![
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/foo/bar")),
            },
        ]);

        let mount_point = mounts
            .find_mount_point(Path::new("/foo/barista/file.txt"))
            .unwrap();

        assert_eq!(mount_point.as_path(), Path::new("/"));
    }

    #[test]
    fn test_home_trash_path() {
        for (xdg_data_home, home, expected) in [
            (
                Some(OsStr::new("/home/user/.local/share")),
                Some(OsStr::new("/home/user")),
                Path::new("/home/user/.local/share/Trash"),
            ),
            (
                Some(OsStr::new(".local/share")),
                Some(OsStr::new("/home/user")),
                Path::new("/home/user/.local/share/Trash"),
            ),
            (
                None,
                Some(OsStr::new("/home/user")),
                Path::new("/home/user/.local/share/Trash"),
            ),
        ] {
            let home_trash = home_trash_path(xdg_data_home, home).unwrap();

            assert_eq!(home_trash, expected);
        }

        for (xdg_data_home, home) in [(None, Some(OsStr::new("home/user"))), (None, None)] {
            let error = home_trash_path(xdg_data_home, home).unwrap_err();

            assert!(matches!(error, Error::Platform { .. }));
        }
    }

    #[test]
    fn test_canonicalize_nearest_existing_parent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("directory");
        let dir_link = temp_dir.path().join("directory-link");
        let trash = dir_link.join(".local/share/Trash");

        std::fs::create_dir(&dir).unwrap();
        unix::fs::symlink(&dir, &dir_link).unwrap();

        let canonical = canonicalize_nearest_existing_parent(&trash).unwrap();

        assert_eq!(
            canonical,
            dir.canonicalize().unwrap().join(".local/share/Trash")
        );
    }

    #[test]
    fn test_external_trash_path_without_shared_trash() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("missing");

        std::fs::create_dir(&top_dir).unwrap();

        let external_trash = external_trash_path(&top_dir, user_id);

        assert_eq!(
            external_trash,
            ExternalTrashPath {
                path: top_dir.join(format!(".Trash-{user_id}")),
                fallback_path: None,
            }
        );
    }

    #[test]
    fn test_external_trash_path_with_valid_shared_trash() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("shared");
        let shared_trash = top_dir.join(".Trash");

        std::fs::create_dir_all(&shared_trash).unwrap();
        chmod(&shared_trash, 0o1777).unwrap();

        let external_trash = external_trash_path(&top_dir, user_id);

        assert_eq!(
            external_trash,
            ExternalTrashPath {
                path: shared_trash.join(user_id.to_string()),
                fallback_path: Some(top_dir.join(format!(".Trash-{user_id}"))),
            }
        );
    }

    #[test]
    fn test_external_trash_path_ignores_symlink_shared_trash() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("symlink");
        let symlink_target = temp_dir.path().join("symlink-target");

        std::fs::create_dir(&top_dir).unwrap();
        std::fs::create_dir(&symlink_target).unwrap();
        unix::fs::symlink(&symlink_target, top_dir.join(".Trash")).unwrap();

        let external_trash = external_trash_path(&top_dir, user_id);

        assert_eq!(
            external_trash,
            ExternalTrashPath {
                path: top_dir.join(format!(".Trash-{user_id}")),
                fallback_path: None,
            }
        );
    }

    #[test]
    fn test_external_trash_path_ignores_non_sticky_shared_trash() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("non-sticky");
        let shared_trash = top_dir.join(".Trash");

        std::fs::create_dir_all(&shared_trash).unwrap();
        chmod(&shared_trash, 0o0777).unwrap();

        let external_trash = external_trash_path(&top_dir, user_id);

        assert_eq!(
            external_trash,
            ExternalTrashPath {
                path: top_dir.join(format!(".Trash-{user_id}")),
                fallback_path: None,
            }
        );
    }

    #[test]
    fn test_prepare_home_trash_location() {
        let temp_dir = TempDir::new().unwrap();
        let home_trash = temp_dir.path().join("home/user/.local/share/Trash");
        let location = TrashLocation::Home {
            path: home_trash.clone(),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };

        let directory = location.prepare().unwrap();

        assert_eq!(
            directory,
            TrashDirectory {
                path: home_trash.to_path_buf(),
                files: home_trash.join("files"),
                info: home_trash.join("info"),
            }
        );
        assert!(home_trash.join("files").is_dir());
        assert!(home_trash.join("info").is_dir());
    }

    #[test]
    fn test_prepare_external_trash_location() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("media/usb");
        let external_trash = top_dir.join(format!(".Trash-{user_id}"));
        let location = TrashLocation::External {
            path: ExternalTrashPath {
                path: external_trash.clone(),
                fallback_path: None,
            },
            mount_point: MountPoint(top_dir.clone()),
        };

        std::fs::create_dir_all(&top_dir).unwrap();

        let directory = location.prepare().unwrap();

        assert_eq!(
            directory,
            TrashDirectory {
                path: external_trash.to_path_buf(),
                files: external_trash.join("files"),
                info: external_trash.join("info"),
            }
        );
        assert!(external_trash.join("files").is_dir());
        assert!(external_trash.join("info").is_dir());
    }

    #[test]
    fn test_prepare_shared_external_trash_location() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("media/usb");
        let shared_trash = top_dir.join(".Trash");
        let external_trash = shared_trash.join(user_id.to_string());
        let fallback_trash = top_dir.join(format!(".Trash-{user_id}"));
        let location = TrashLocation::External {
            path: ExternalTrashPath {
                path: external_trash.clone(),
                fallback_path: Some(fallback_trash.clone()),
            },
            mount_point: MountPoint(top_dir),
        };

        std::fs::create_dir_all(&shared_trash).unwrap();
        chmod(&shared_trash, 0o1777).unwrap();

        let directory = location.prepare().unwrap();

        assert_eq!(
            directory,
            TrashDirectory {
                path: external_trash.to_path_buf(),
                files: external_trash.join("files"),
                info: external_trash.join("info"),
            }
        );
        assert!(external_trash.join("files").is_dir());
        assert!(external_trash.join("info").is_dir());
        assert!(!fallback_trash.exists());
    }

    #[test]
    fn test_prepare_shared_external_trash_location_with_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("media/usb");
        let shared_trash = top_dir.join(".Trash");
        let external_trash = shared_trash.join(user_id.to_string());
        let fallback_trash = top_dir.join(format!(".Trash-{user_id}"));
        let location = TrashLocation::External {
            path: ExternalTrashPath {
                path: external_trash.clone(),
                fallback_path: Some(fallback_trash.clone()),
            },
            mount_point: MountPoint(top_dir),
        };

        std::fs::create_dir_all(&shared_trash).unwrap();
        chmod(&shared_trash, 0o1777).unwrap();
        std::fs::write(&external_trash, b"file instead of directory").unwrap();

        let directory = location.prepare().unwrap();

        assert_eq!(
            directory,
            TrashDirectory {
                path: fallback_trash.to_path_buf(),
                files: fallback_trash.join("files"),
                info: fallback_trash.join("info"),
            }
        );
        assert!(fallback_trash.join("files").is_dir());
        assert!(fallback_trash.join("info").is_dir());
    }

    #[test]
    fn test_prepare_shared_external_trash_location_with_missing_parent() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("media/usb");
        let missing_shared_trash = top_dir.join(".Trash");
        let fallback_trash = top_dir.join(format!(".Trash-{user_id}"));
        let location = TrashLocation::External {
            path: ExternalTrashPath {
                path: missing_shared_trash.join(user_id.to_string()),
                fallback_path: Some(fallback_trash.clone()),
            },
            mount_point: MountPoint(top_dir),
        };

        let directory = location.prepare().unwrap();

        assert_eq!(
            directory,
            TrashDirectory {
                path: fallback_trash.to_path_buf(),
                files: fallback_trash.join("files"),
                info: fallback_trash.join("info"),
            }
        );
        assert!(fallback_trash.join("files").is_dir());
        assert!(fallback_trash.join("info").is_dir());
        assert!(!missing_shared_trash.exists());
    }

    #[test]
    fn test_select_trash_location() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let home_mount = temp_dir.path().join("home");
        let external_mount = temp_dir.path().join("media/usb");
        let home_trash = home_mount.join("user/.local/share/Trash");

        std::fs::create_dir_all(&home_mount).unwrap();
        std::fs::create_dir_all(&external_mount).unwrap();

        let mounts = Mounts::new(vec![
            MountInfo {
                mount_point: MountPoint(temp_dir.path().to_path_buf()),
            },
            MountInfo {
                mount_point: MountPoint(home_mount.clone()),
            },
            MountInfo {
                mount_point: MountPoint(external_mount.clone()),
            },
        ]);
        let location = TrashLocation::select(
            &home_mount.join("user/file.txt"),
            &mounts,
            &home_trash,
            user_id,
        )
        .unwrap();

        assert_eq!(
            location,
            TrashLocation::Home {
                path: home_trash.clone(),
                mount_point: MountPoint(home_mount.clone()),
            }
        );

        let location = TrashLocation::select(
            &external_mount.join("file.txt"),
            &mounts,
            &home_trash,
            user_id,
        )
        .unwrap();

        assert_eq!(
            location,
            TrashLocation::External {
                path: ExternalTrashPath {
                    path: external_mount.join(format!(".Trash-{user_id}")),
                    fallback_path: None,
                },
                mount_point: MountPoint(external_mount),
            }
        );
    }
}
