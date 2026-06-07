use std::{
    collections::HashSet,
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use super::{
    location::TrashDirectory,
    payload,
    trash_info::{self, ReservedTrashEntry},
};
use crate::{Error, Result};

const STAT_BLOCK_SIZE: u64 = 512;

#[derive(Debug)]
pub(super) struct DirectorySizeCache {
    path: PathBuf,
}

impl DirectorySizeCache {
    pub(super) fn new(trash_dir: &TrashDirectory) -> Self {
        Self {
            path: trash_dir.path.join("directorysizes"),
        }
    }

    pub(super) fn update(&self, entry: &ReservedTrashEntry) -> Result<()> {
        let size = self.disk_usage(entry)?;
        let mtime = entry
            .info
            .metadata()
            .map_err(|source| Error::Io {
                path: entry.info.clone(),
                source,
            })?
            .mtime();
        let name = trash_info::percent_encode_path(Path::new(entry.name.as_os_str()));
        let contents = self.contents(size, mtime, &name)?;

        self.write(&contents)
    }

    fn contents(&self, size: u64, mtime: i64, name: &str) -> Result<String> {
        let mut contents = String::new();

        match std::fs::read_to_string(&self.path) {
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
                    path: self.path.clone(),
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

    fn write(&self, contents: &str) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| Error::Platform {
            message: format!(
                "Directory size cache path has no parent: {}",
                self.path.display()
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
                                    self.path.display()
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

            if let Err(source) = std::fs::rename(&temporary_file, &self.path) {
                let rename_path = self.path.clone();
                payload::remove_file_if_exists(&temporary_file)?;

                return Err(Error::Io {
                    path: rename_path,
                    source,
                });
            }

            return Ok(());
        }
    }

    pub(super) fn disk_usage(&self, entry: &ReservedTrashEntry) -> Result<u64> {
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

        inner(&entry.file, &mut HashSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;
    use std::os::unix::{self, fs::MetadataExt};
    use tempfile::TempDir;

    #[test]
    fn test_cache_contents_replaces_existing_entry() {
        let temp_dir = TempDir::new().unwrap();
        let cache = DirectorySizeCache {
            path: temp_dir.path().join("directorysizes"),
        };

        std::fs::write(
            &cache.path,
            indoc! {"
                2048 1 Other
                1024 2 Camera
            "},
        )
        .unwrap();

        let contents = cache.contents(4096, 1_779_555_000, "Camera").unwrap();

        assert_eq!(
            contents,
            indoc! {"
                2048 1 Other
                4096 1779555000 Camera
            "}
        );
    }

    #[test]
    fn test_disk_usage_does_not_follow_directory_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path().join("project");
        let shared_dir = temp_dir.path().join("shared");
        let shared_link = project_dir.join("shared-link");

        std::fs::create_dir(&project_dir).unwrap();
        std::fs::create_dir(&shared_dir).unwrap();
        std::fs::write(shared_dir.join("LICENSE"), b"license text").unwrap();
        unix::fs::symlink(&shared_dir, &shared_link).unwrap();

        let cache = DirectorySizeCache {
            path: temp_dir.path().join("directorysizes"),
        };
        let entry = ReservedTrashEntry {
            name: "project".into(),
            file: project_dir.clone(),
            info: temp_dir.path().join("project.trashinfo"),
        };
        let size = cache.disk_usage(&entry).unwrap();
        let expected_size = (project_dir.symlink_metadata().unwrap().blocks()
            + shared_link.symlink_metadata().unwrap().blocks())
            * STAT_BLOCK_SIZE;

        assert_eq!(size, expected_size);
    }
}
