use rustix::process;
use std::{
    env,
    ffi::OsStr,
    io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use super::{
    mount::{MountPoint, Mounts},
    permission::{self, OWNER_RWX_MODE},
};
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TrashDirectory {
    pub(super) path: PathBuf,
    pub(super) files: PathBuf,
    pub(super) info: PathBuf,
}

impl TrashDirectory {
    fn new(path: PathBuf) -> Self {
        let files = path.join("files");
        let info = path.join("info");

        Self { path, files, info }
    }

    pub(super) fn prepare(path: &Path) -> Result<Self> {
        let trash_dir = Self::new(path.to_owned());

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

    fn prepare_home(path: &Path) -> Result<Self> {
        fn ensure_directory(path: &Path) -> Result<()> {
            let metadata = path.metadata().map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;

            if !metadata.file_type().is_dir() {
                return Err(Error::Io {
                    path: path.to_owned(),
                    source: io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "path exists but is not a directory",
                    ),
                });
            }

            Ok(())
        }

        fn create_directory(path: &Path) -> Result<()> {
            match std::fs::create_dir(path) {
                Ok(()) => {
                    permission::set_mode(path, OWNER_RWX_MODE)?;
                    return Ok(());
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    return ensure_directory(path);
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

            create_directory(parent)?;

            match std::fs::create_dir(path) {
                Ok(()) => permission::set_mode(path, OWNER_RWX_MODE),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    ensure_directory(path)
                }
                Err(source) => Err(Error::Io {
                    path: path.to_owned(),
                    source,
                }),
            }
        }

        let trash_dir = Self::new(path.to_owned());

        for path in [
            trash_dir.path.as_path(),
            trash_dir.files.as_path(),
            trash_dir.info.as_path(),
        ] {
            create_directory(path)?;
        }

        Ok(trash_dir)
    }

    fn prepare_without_creating_parent(path: &Path) -> Result<Self> {
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
        let has_sticky_bit = metadata.mode() & permission::STICKY_BIT != 0;

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

        let trash_dir = Self::new(path.to_owned());

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HomeTrashPath(pub(super) PathBuf);

impl HomeTrashPath {
    fn resolve(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<Self> {
        if let Some(xdg_data_home) = xdg_data_home {
            if !xdg_data_home.is_empty() {
                let xdg_data_home = Path::new(xdg_data_home);

                if xdg_data_home.is_absolute() {
                    return Ok(Self(xdg_data_home.join("Trash")));
                }
            }
        }

        if let Some(home) = home {
            if !home.is_empty() {
                let home = Path::new(home);

                if home.is_absolute() {
                    return Ok(Self(home.join(".local/share/Trash")));
                }
            }
        }

        Err(Error::Platform {
            message: "No absolute XDG_DATA_HOME or HOME is available".to_owned(),
        })
    }

    fn canonicalize_existing_parent(self) -> Result<Self> {
        for ancestor in self.0.ancestors() {
            match ancestor.canonicalize() {
                Ok(mut canonical) => {
                    let suffix =
                        self.0
                            .strip_prefix(ancestor)
                            .map_err(|source| Error::Platform {
                                message: format!(
                                    "Failed to resolve {}: {source}",
                                    self.0.display()
                                ),
                            })?;

                    canonical.push(suffix);
                    return Ok(Self(canonical));
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
            message: format!("Path has no existing parent: {}", self.0.display()),
        })
    }

    fn prepare(&self) -> Result<TrashDirectory> {
        TrashDirectory::prepare_home(self.as_path())
    }

    fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExternalTrashPath {
    pub(super) path: PathBuf,
    pub(super) fallback_path: Option<PathBuf>,
}

impl ExternalTrashPath {
    fn new(top_dir: &Path, user_id: u32) -> Self {
        let fallback_trash = top_dir.join(format!(".Trash-{user_id}"));
        let shared_trash = top_dir.join(".Trash");

        if let Ok(metadata) = shared_trash.symlink_metadata() {
            let file_type = metadata.file_type();
            let has_sticky_bit = metadata.mode() & permission::STICKY_BIT != 0;

            if file_type.is_dir() && !file_type.is_symlink() && has_sticky_bit {
                return Self {
                    path: shared_trash.join(user_id.to_string()),
                    fallback_path: Some(fallback_trash),
                };
            }
        }

        Self {
            path: fallback_trash,
            fallback_path: None,
        }
    }

    fn prepare(&self) -> Result<TrashDirectory> {
        match &self.fallback_path {
            Some(fallback) => TrashDirectory::prepare_without_creating_parent(&self.path)
                .or_else(|_| TrashDirectory::prepare(fallback)),
            None => TrashDirectory::prepare(&self.path),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TrashLocation {
    Home {
        path: HomeTrashPath,
        mount_point: MountPoint,
    },
    External {
        path: ExternalTrashPath,
        mount_point: MountPoint,
    },
}

impl TrashLocation {
    pub(super) fn resolve(path: &Path) -> Result<Self> {
        let mounts = Mounts::read()?;
        let user_id = process::getuid().as_raw();
        let xdg_data_home = env::var_os("XDG_DATA_HOME");
        let home = env::var_os("HOME");
        let home_trash = HomeTrashPath::resolve(xdg_data_home.as_deref(), home.as_deref())?
            .canonicalize_existing_parent()?;

        Self::select(path, &mounts, &home_trash, user_id)
    }

    fn select(
        path: &Path,
        mounts: &Mounts,
        home_trash: &HomeTrashPath,
        user_id: u32,
    ) -> Result<Self> {
        let target_mount = mounts
            .find_mount_point(path)
            .ok_or_else(|| Error::Platform {
                message: format!("No mount point found for {}", path.display()),
            })?;

        if path == target_mount.as_path() {
            return Err(Error::TargetedRoot {
                path: path.to_path_buf(),
            });
        }

        let home_mount = mounts
            .find_mount_point(home_trash.as_path())
            .ok_or_else(|| Error::Platform {
                message: format!(
                    "No mount point found for {}",
                    home_trash.as_path().display()
                ),
            })?;

        if target_mount == home_mount {
            return Ok(Self::Home {
                path: home_trash.clone(),
                mount_point: target_mount,
            });
        }

        Ok(Self::External {
            path: ExternalTrashPath::new(target_mount.as_path(), user_id),
            mount_point: target_mount,
        })
    }

    pub(super) fn prepare(&self) -> Result<TrashDirectory> {
        match self {
            Self::Home { path, .. } => path.prepare(),
            Self::External { path, .. } => path.prepare(),
        }
    }

    pub(super) fn trash_info_path(&self, path: &Path) -> Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        ffi::OsStr,
        os::unix::{self, fs::MetadataExt},
        path::{Path, PathBuf},
    };
    use tempfile::TempDir;

    use crate::platform::linux::{
        mount::MountInfo,
        permission::{
            OWNER_RWX_WORLD_RX_MODE, PERMISSION_BITS_MASK, WORLD_RWX_MODE, WORLD_RWX_STICKY_MODE,
        },
    };

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
            let home_trash = HomeTrashPath::resolve(xdg_data_home, home).unwrap();

            assert_eq!(home_trash.as_path(), expected);
        }

        for (xdg_data_home, home) in [(None, Some(OsStr::new("home/user"))), (None, None)] {
            let error = HomeTrashPath::resolve(xdg_data_home, home).unwrap_err();

            assert!(matches!(error, Error::Platform { .. }));
        }
    }

    #[test]
    fn test_home_trash_path_canonicalizes_existing_parent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("directory");
        let dir_link = temp_dir.path().join("directory-link");
        let trash = dir_link.join(".local/share/Trash");

        std::fs::create_dir(&dir).unwrap();
        unix::fs::symlink(&dir, &dir_link).unwrap();

        let canonical = HomeTrashPath(trash).canonicalize_existing_parent().unwrap();
        let expected = dir.canonicalize().unwrap().join(".local/share/Trash");

        assert_eq!(canonical.as_path(), expected.as_path());
    }

    #[test]
    fn test_external_trash_path_without_shared_trash() {
        let temp_dir = TempDir::new().unwrap();
        let user_id = 1000;
        let top_dir = temp_dir.path().join("media/missing-usb");

        std::fs::create_dir_all(&top_dir).unwrap();

        let external_trash = ExternalTrashPath::new(&top_dir, user_id);

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
        permission::set_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();

        let external_trash = ExternalTrashPath::new(&top_dir, user_id);

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

        std::fs::create_dir_all(&top_dir).unwrap();
        std::fs::create_dir(&trash_link_target).unwrap();
        unix::fs::symlink(&trash_link_target, &trash_link).unwrap();

        let external_trash = ExternalTrashPath::new(&top_dir, user_id);

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
        permission::set_mode(&shared_trash, WORLD_RWX_MODE).unwrap();

        let external_trash = ExternalTrashPath::new(&top_dir, user_id);

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
            path: HomeTrashPath(home_trash.clone()),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: home_trash.clone(),
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
            path: HomeTrashPath(home_trash.clone()),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };

        std::fs::create_dir_all(&home_trash).unwrap();
        permission::set_mode(&home_trash, OWNER_RWX_WORLD_RX_MODE).unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: home_trash.clone(),
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
    fn test_prepare_home_trash_location_with_directory_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let home_trash = temp_dir.path().join("home/user/.local/share/Trash");
        let linked_trash = temp_dir.path().join("linked-trash");
        let location = TrashLocation::Home {
            path: HomeTrashPath(home_trash.clone()),
            mount_point: MountPoint(temp_dir.path().to_owned()),
        };

        std::fs::create_dir_all(home_trash.parent().unwrap()).unwrap();
        std::fs::create_dir(&linked_trash).unwrap();
        unix::fs::symlink(&linked_trash, &home_trash).unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: home_trash.clone(),
                files: home_trash.join("files"),
                info: home_trash.join("info"),
            }
        );
        assert!(linked_trash.join("files").is_dir());
        assert!(linked_trash.join("info").is_dir());
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
                path: external_trash.clone(),
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
        permission::set_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: external_trash.clone(),
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
        permission::set_mode(&shared_trash, WORLD_RWX_STICKY_MODE).unwrap();
        std::fs::write(&external_trash, b"file instead of directory").unwrap();

        let trash_dir = location.prepare().unwrap();

        assert_eq!(
            trash_dir,
            TrashDirectory {
                path: fallback_trash.clone(),
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
                path: fallback_trash.clone(),
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
        let home_trash = HomeTrashPath(home_mount.join("user/.local/share/Trash"));

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

        let error = TrashLocation::select(&home_mount, &mounts, &home_trash, user_id).unwrap_err();
        assert!(matches!(error, Error::TargetedRoot { .. }));

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
                mount_point: MountPoint(external_mount.clone()),
            }
        );

        let error =
            TrashLocation::select(&external_mount, &mounts, &home_trash, user_id).unwrap_err();
        assert!(matches!(error, Error::TargetedRoot { .. }));
    }

    #[test]
    fn test_trash_info_path() {
        let user_id = 1000;
        let home_location = TrashLocation::Home {
            path: HomeTrashPath(PathBuf::from("/home/user/.local/share/Trash")),
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
            .trash_info_path(Path::new("/media/usb/Photos/image.jpg"))
            .unwrap();
        let invalid_parent_component_error = external_location
            .trash_info_path(Path::new("/media/usb/Photos/../image.jpg"))
            .unwrap_err();

        assert_eq!(
            home_original_location,
            PathBuf::from("/home/user/Downloads/file.txt")
        );
        assert_eq!(
            external_original_location,
            PathBuf::from("Photos/image.jpg")
        );
        assert!(matches!(
            invalid_parent_component_error,
            Error::Platform { .. }
        ));
    }
}
