use indoc::formatdoc;
use rustix::process;
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};
use time::OffsetDateTime;

use crate::{Error, Result, Trash, TrashItem};

const MOUNT_INFO_PATH: &str = "/proc/self/mountinfo";
const OWNER_RWX_MODE: u32 = 0o700;
const STICKY_BIT: u32 = 0o1000;

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
struct ReservedTrashEntry {
    name: OsString,
    file: PathBuf,
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
            Self::Home { path, .. } => prepare_home_trash_directory(path),
            Self::External { path, .. } => prepare_external_trash_directory(path),
        }
    }

    fn trash_info_path(&self, path: &Path) -> Result<PathBuf> {
        match self {
            Self::Home { .. } => Ok(path.to_path_buf()),
            Self::External { mount_point, .. } => {
                let original_location =
                    path.strip_prefix(mount_point.as_path())
                        .map_err(|source| Error::Platform {
                            message: format!(
                                "Failed to make {} relative to {}: {source}",
                                path.display(),
                                mount_point.as_path().display()
                            ),
                        })?;

                if original_location.as_os_str().is_empty() {
                    return Err(Error::Platform {
                        message: format!(
                            "Trash info original location is empty for {}",
                            path.display()
                        ),
                    });
                }

                if original_location
                    .components()
                    .any(|component| component == Component::ParentDir)
                {
                    return Err(Error::Platform {
                        message: format!(
                            "Trash info path must not contain '..': {}",
                            original_location.display()
                        ),
                    });
                }

                Ok(original_location.to_path_buf())
            }
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
        let has_sticky_bit = metadata.mode() & STICKY_BIT != 0;

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
    let trash_dir = TrashDirectory {
        path: path.to_owned(),
        files: path.join("files"),
        info: path.join("info"),
    };

    for path in [
        trash_dir.path.as_path(),
        trash_dir.files.as_path(),
        trash_dir.info.as_path(),
    ] {
        std::fs::create_dir_all(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
    }

    Ok(trash_dir)
}

fn prepare_home_trash_directory(path: &Path) -> Result<TrashDirectory> {
    let trash_dir = TrashDirectory {
        path: path.to_owned(),
        files: path.join("files"),
        info: path.join("info"),
    };

    for path in [
        trash_dir.path.as_path(),
        trash_dir.files.as_path(),
        trash_dir.info.as_path(),
    ] {
        create_dir_all_with_permissions(path, OWNER_RWX_MODE)?;
    }

    Ok(trash_dir)
}

fn create_dir_all_with_permissions(path: &Path, mode: u32) -> Result<()> {
    let validate_existing_dir = |path: &Path| -> Result<()> {
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

        Ok(())
    };

    match std::fs::create_dir(path) {
        Ok(()) => {
            set_permissions_mode(path, mode)?;
            return Ok(());
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            return validate_existing_dir(path);
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    }

    let Some(parent) = path.parent() else {
        return Err(Error::Platform {
            message: format!("Path has no parent: {}", path.display()),
        });
    };

    create_dir_all_with_permissions(parent, mode)?;

    match std::fs::create_dir(path) {
        Ok(()) => set_permissions_mode(path, mode),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => validate_existing_dir(path),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn set_permissions_mode(path: &Path, mode: u32) -> Result<()> {
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
    let has_sticky_bit = metadata.mode() & STICKY_BIT != 0;

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

    let trash_dir = TrashDirectory {
        path: path.to_owned(),
        files: path.join("files"),
        info: path.join("info"),
    };

    for path in [
        trash_dir.path.as_path(),
        trash_dir.files.as_path(),
        trash_dir.info.as_path(),
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

    Ok(trash_dir)
}

fn prepare_external_trash_directory(path: &ExternalTrashPath) -> Result<TrashDirectory> {
    match &path.fallback_path {
        Some(fallback) => prepare_trash_directory_without_creating_parent(&path.path)
            .or_else(|_| prepare_trash_directory(fallback)),
        None => prepare_trash_directory(&path.path),
    }
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

    use indoc::indoc;
    use std::os::unix::{self, fs::MetadataExt};
    use tempfile::TempDir;

    const WORLD_RWX_STICKY_MODE: u32 = 0o1777;
    const WORLD_RWX_MODE: u32 = 0o0777;
    const OWNER_RWX_WORLD_RX_MODE: u32 = 0o755;
    const PERMISSION_BITS_MASK: u32 = 0o777;

    #[test]
    fn test_parse_mount_info() {
        let bytes = indoc! {b"
            36 35 98:0 / / rw,relatime - ext4 /dev/root rw
            37 36 8:1 / /home rw,nosuid - ext4 /dev/sda1 rw
        "};
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
        let bytes = b"37 36 8:1 / /media/USB\\040Drive rw - ext4 /dev/sda1 rw";
        let entries = parse_mount_info(bytes).unwrap();

        assert_eq!(
            entries,
            vec![MountInfo {
                mount_point: MountPoint(PathBuf::from("/media/USB Drive")),
            }]
        );
    }

    #[test]
    fn test_parse_mount_info_preserves_invalid_utf8() {
        let bytes = b"37 36 8:1 / /media/camera-\\377card rw - ext4 /dev/sda1 rw";
        let entries = parse_mount_info(bytes).unwrap();

        assert_eq!(
            entries,
            vec![MountInfo {
                mount_point: MountPoint(PathBuf::from(OsString::from_vec(
                    b"/media/camera-\xffcard".to_vec()
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
                mount_point: MountPoint(PathBuf::from("/home")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/home/user")),
            },
        ]);

        let mount_point = mounts
            .find_mount_point(Path::new("/home/user/Downloads/file.txt"))
            .unwrap();

        assert_eq!(mount_point.as_path(), Path::new("/home/user"));
    }

    #[test]
    fn test_find_mount_point_does_not_match_partial_component() {
        let mounts = Mounts::new(vec![
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/")),
            },
            MountInfo {
                mount_point: MountPoint(PathBuf::from("/media/user/External")),
            },
        ]);

        let mount_point = mounts
            .find_mount_point(Path::new("/media/user/ExternalSSD/file.txt"))
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
        let top_dir = temp_dir.path().join("media/missing-usb");

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
        let top_dir = temp_dir.path().join("media/usb");
        let shared_trash = top_dir.join(".Trash");

        std::fs::create_dir_all(&shared_trash).unwrap();
        set_permissions_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();

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
        let top_dir = temp_dir.path().join("media/usb");
        let trash_link = top_dir.join(".Trash");
        let trash_link_target = temp_dir.path().join("target-usb");

        std::fs::create_dir(&top_dir).unwrap();
        std::fs::create_dir(&trash_link_target).unwrap();
        unix::fs::symlink(&trash_link_target, &trash_link).unwrap();

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
        let top_dir = temp_dir.path().join("media/usb");
        let shared_trash = top_dir.join(".Trash");

        std::fs::create_dir_all(&shared_trash).unwrap();
        set_permissions_mode(&shared_trash, WORLD_RWX_MODE).unwrap();

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

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: home_trash.to_path_buf(),
                files: home_trash.join("files"),
                info: home_trash.join("info"),
            }
        );
        assert!(home_trash.join("files").is_dir());
        assert!(home_trash.join("info").is_dir());
        assert_eq!(
            home_trash.metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
        assert_eq!(
            home_trash.join("files").metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
        assert_eq!(
            home_trash.join("info").metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
    }

    #[test]
    fn test_prepare_home_trash_location_does_not_change_existing_directory_permissions() {
        let temp_dir = TempDir::new().unwrap();
        let home_trash = temp_dir.path().join("home/user/.local/share/Trash");
        let location = TrashLocation::Home {
            path: home_trash.clone(),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };

        std::fs::create_dir_all(&home_trash).unwrap();
        set_permissions_mode(&home_trash, OWNER_RWX_WORLD_RX_MODE).unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: home_trash.to_path_buf(),
                files: home_trash.join("files"),
                info: home_trash.join("info"),
            }
        );
        assert_eq!(
            home_trash.metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_WORLD_RX_MODE
        );
        assert_eq!(
            home_trash.join("files").metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
        assert_eq!(
            home_trash.join("info").metadata().unwrap().mode() & PERMISSION_BITS_MASK,
            OWNER_RWX_MODE
        );
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

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
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
        set_permissions_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
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
        set_permissions_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();
        std::fs::write(&external_trash, b"file instead of directory").unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
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

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
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
    fn test_create_trash_info_with_duplicates() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let trash_dir = prepare_trash_directory(&trash).unwrap();
        let location = TrashLocation::Home {
            path: trash.clone(),
            mount_point: MountPoint(PathBuf::from("/home")),
        };
        let original_path = Path::new("/home/user/file.txt");

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
                Path=/home/user/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
        assert_eq!(
            std::fs::read_to_string(&second.info).unwrap(),
            indoc! {"
                [Trash Info]
                Path=/home/user/file.txt
                DeletionDate=2026-05-23T16:50:00
            "}
        );
    }

    #[test]
    fn test_create_trash_info_with_external_location() {
        let temp_dir = TempDir::new().unwrap();
        let trash = temp_dir.path().join("Trash");
        let trash_dir = prepare_trash_directory(&trash).unwrap();
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

    #[test]
    fn test_trash_info_path() {
        let user_id = 1000;
        let home_location = TrashLocation::Home {
            path: PathBuf::from("/home/user/.local/share/Trash"),
            mount_point: MountPoint(PathBuf::from("/home")),
        };
        let external_location = TrashLocation::External {
            path: ExternalTrashPath {
                path: PathBuf::from(format!("/media/usb/.Trash-{user_id}")),
                fallback_path: None,
            },
            mount_point: MountPoint(PathBuf::from("/media/usb")),
        };

        let home_original_location = home_location
            .trash_info_path(Path::new("/home/user/Downloads/file.txt"))
            .unwrap();
        let external_original_location = external_location
            .trash_info_path(Path::new("/media/usb/Photos/image.png"))
            .unwrap();
        let invalid_parent_component_error = external_location
            .trash_info_path(Path::new("/media/usb/Photos/../image.png"))
            .unwrap_err();

        assert_eq!(
            home_original_location,
            PathBuf::from("/home/user/Downloads/file.txt")
        );
        assert_eq!(
            external_original_location,
            PathBuf::from("Photos/image.png")
        );
        assert!(matches!(
            invalid_parent_component_error,
            Error::Platform { .. }
        ));
    }
}
