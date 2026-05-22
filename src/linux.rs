use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
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
            path: PathBuf::from(MOUNT_INFO_PATH),
            message: format!("Failed to parse mount info: {source:?}"),
        })?;

        if entries.is_empty() {
            return Err(Error::Platform {
                path: PathBuf::from(MOUNT_INFO_PATH),
                message: "No mount points found".to_owned(),
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
enum TrashLocation {
    Home {
        path: PathBuf,
        mount_point: MountPoint,
    },
    External {
        path: PathBuf,
        mount_point: MountPoint,
    },
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
}
