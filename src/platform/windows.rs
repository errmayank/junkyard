mod progress_sink;
mod sta_thread;

use std::{
    ffi::OsStr,
    io,
    marker::PhantomData,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf, Prefix},
    time::SystemTime,
};
use windows::Win32::{
    System::Com::{self, CLSCTX_ALL},
    UI::Shell::{
        self, FOF_NO_CONNECTED_ELEMENTS, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
        FOFX_ADDUNDORECORD, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
        IFileOperationProgressSink, IShellItem,
    },
};
use windows_core::{GUID, PCWSTR};

use progress_sink::RecycleProgressSink;
use sta_thread::{ComApartment, run_on_sta_thread};

use crate::{Error, Result, Trash, TrashItem};

pub(crate) fn discard(_: &Trash, path: &Path) -> Result<TrashItem> {
    let path = path.to_path_buf();

    run_on_sta_thread(move |com_apartment| discard_inner(com_apartment, &path))
}

pub(crate) fn discard_all(_: &Trash, paths: &[PathBuf]) -> Result<Vec<TrashItem>> {
    let paths = paths.to_vec();

    run_on_sta_thread(move |com_apartment| {
        paths
            .iter()
            .map(|path| discard_inner(com_apartment, path))
            .collect()
    })
}

fn discard_inner(com_apartment: &ComApartment, path: &Path) -> Result<TrashItem> {
    let original_name = path
        .file_name()
        .ok_or_else(|| Error::TargetedRoot {
            path: path.to_path_buf(),
        })?
        .to_os_string();
    let original_parent = path
        .parent()
        .ok_or_else(|| Error::TargetedRoot {
            path: path.to_path_buf(),
        })?
        .to_path_buf();
    let shell_item = shell_item_from_path(com_apartment, path)?;
    let progress_sink = RecycleProgressSink::new();
    let file_operation_progress_sink = progress_sink.to_file_operation_progress_sink();
    let operation = ShellFileOperation::new(com_apartment, path)?;

    operation.set_recycle_flags(path)?;
    operation.queue_delete_item(path, &shell_item, &file_operation_progress_sink)?;
    operation.perform_queued_operations(path, &progress_sink)?;

    let recycled_id = progress_sink.recycled_item_id(path)?;

    Ok(TrashItem::new(
        recycled_id,
        original_name,
        original_parent,
        SystemTime::now(),
    ))
}

#[derive(Debug)]
struct ShellFileOperation<'a> {
    operation: IFileOperation,
    _com_apartment: PhantomData<&'a ComApartment>,
}

impl<'a> ShellFileOperation<'a> {
    fn new(_com_apartment: &'a ComApartment, path: &Path) -> Result<Self> {
        // SAFETY: `_com_apartment` guarantees COM is initialized on this thread and
        // `FileOperation` is the Shell-provided CLSID for `IFileOperation`.
        let operation: IFileOperation = unsafe {
            Com::CoCreateInstance(std::ptr::from_ref::<GUID>(&FileOperation), None, CLSCTX_ALL)
        }
        .map_err(|source| Error::Platform {
            message: format!(
                "Failed to create Windows file operation for {}: {source}",
                path.display()
            ),
        })?;

        Ok(Self {
            operation,
            _com_apartment: PhantomData,
        })
    }

    fn set_recycle_flags(&self, path: &Path) -> Result<()> {
        let operation_flags = FOFX_RECYCLEONDELETE
            | FOFX_ADDUNDORECORD
            | FOFX_EARLYFAILURE
            | FOF_NOERRORUI
            | FOF_NO_CONNECTED_ELEMENTS
            | FOF_SILENT
            | FOF_WANTNUKEWARNING;

        // SAFETY: `self.operation` comes from a successful `CoCreateInstance` call and
        // `SetOperationFlags` only reads the interface pointer for this call.
        unsafe { self.operation.SetOperationFlags(operation_flags) }.map_err(|source| {
            Error::Platform {
                message: format!(
                    "Failed to configure Windows recycle operation for {}: {source}",
                    path.display()
                ),
            }
        })
    }

    fn queue_delete_item(
        &self,
        path: &Path,
        shell_item: &IShellItem,
        progress_sink: &IFileOperationProgressSink,
    ) -> Result<()> {
        // SAFETY: `self.operation` and `shell_item` are valid COM interfaces and
        // `progress_sink` remains alive for the duration of the queued operation.
        unsafe { self.operation.DeleteItem(shell_item, progress_sink) }.map_err(|source| {
            Error::Platform {
                message: format!(
                    "Failed to queue Windows recycle operation for {}: {source}",
                    path.display()
                ),
            }
        })
    }

    fn perform_queued_operations(
        &self,
        path: &Path,
        progress_sink: &RecycleProgressSink,
    ) -> Result<()> {
        // SAFETY: `self.operation` comes from a successful `CoCreateInstance` call and
        // the queued item and progress sink remain alive for the duration of this call.
        let operation_result = unsafe { self.operation.PerformOperations() };

        // SAFETY: `self.operation` comes from a successful `CoCreateInstance` call and
        // remains valid after `PerformOperations` returns.
        let operation_aborted = unsafe { self.operation.GetAnyOperationsAborted() }
            .map(windows_core::BOOL::as_bool)
            .map_err(|source| Error::Platform {
                message: format!(
                    "Failed to inspect Windows recycle operation for {}: {source}",
                    path.display()
                ),
            });
        let operation_aborted = match operation_aborted {
            Ok(operation_aborted) => operation_aborted,
            Err(inspect_error) => match operation_result {
                Ok(()) => return Err(inspect_error),
                Err(source) => {
                    if let Some(message) = progress_sink.failure_message().map_err(
                        |progress_error| Error::Platform {
                            message: format!(
                                "Failed to perform Windows recycle operation for {}; recycle progress state was poisoned: {progress_error}; operation error: {source}",
                                path.display()
                            ),
                        },
                    )?
                    {
                        return Err(Error::Platform {
                            message: format!(
                                "Failed to perform Windows recycle operation for {}: {message}; failed to inspect aborted state: {inspect_error}",
                                path.display()
                            ),
                        });
                    }

                    return Err(Error::Platform {
                        message: format!(
                            "Failed to perform Windows recycle operation for {}: {source}; failed to inspect aborted state: {inspect_error}",
                            path.display()
                        ),
                    });
                }
            },
        };

        match operation_result {
            Ok(()) if operation_aborted => Err(Error::Platform {
                message: format!(
                    "Windows recycle operation was aborted for {}",
                    path.display()
                ),
            }),
            Ok(()) => Ok(()),
            Err(source) => {
                if let Some(message) = progress_sink.failure_message().map_err(
                    |progress_error| Error::Platform {
                        message: format!(
                            "Failed to perform Windows recycle operation for {}; recycle progress state was poisoned: {progress_error}; operation error: {source}",
                            path.display()
                        ),
                    },
                )?
                {
                    return Err(Error::Platform {
                        message: format!(
                            "Failed to perform Windows recycle operation for {}: {message}",
                            path.display()
                        ),
                    });
                }

                if operation_aborted {
                    return Err(Error::Platform {
                        message: format!(
                            "Windows recycle operation was aborted for {}: {source}",
                            path.display()
                        ),
                    });
                }

                Err(Error::Platform {
                    message: format!(
                        "Failed to perform Windows recycle operation for {}: {source}",
                        path.display()
                    ),
                })
            }
        }
    }
}

fn shell_item_from_path(_com_apartment: &ComApartment, path: &Path) -> Result<IShellItem> {
    let create_item = |wide_path: &[u16]| {
        // SAFETY: `_com_apartment` guarantees COM is initialized for the current
        // thread and `wide_path` is a NUL-terminated UTF-16 path that remains
        // valid for the duration of this call.
        unsafe { Shell::SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None) }
    };

    if let Some(compatible_path) = shell_path(path) {
        let compatible_wide_path = nul_terminated_wide_path(&compatible_path)?;

        match create_item(&compatible_wide_path) {
            Ok(shell_item) => Ok(shell_item),
            Err(shell_path_error) => {
                let original_wide_path = nul_terminated_wide_path(path)?;

                create_item(&original_wide_path).map_err(|path_error| {
                    Error::Platform {
                        message: format!(
                            "Failed to create Windows shell item for {}: {shell_path_error}; {path_error}",
                            path.display()
                        ),
                    }
                })
            }
        }
    } else {
        let original_wide_path = nul_terminated_wide_path(path)?;

        create_item(&original_wide_path).map_err(|path_error| Error::Platform {
            message: format!(
                "Failed to create Windows shell item for {}: {path_error}",
                path.display()
            ),
        })
    }
}

fn nul_terminated_wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();

    if wide_path.contains(&0) {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "path contains an interior NUL"),
        });
    }

    wide_path.push(0);

    Ok(wide_path)
}

fn shell_path(path: &Path) -> Option<PathBuf> {
    const LEGACY_PATH_CODE_UNIT_LIMIT_WITH_NUL: usize = 260;

    let mut components = path.components();

    let drive = match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::VerbatimDisk(drive) => drive,
            _ => return None,
        },
        _ => return None,
    };

    if !matches!(components.next()?, Component::RootDir) {
        return None;
    }

    let mut shell_path = PathBuf::from(format!("{}:\\", char::from(drive)));

    for component in components {
        match component {
            Component::Normal(file_name) => {
                if !is_legacy_file_name(file_name) {
                    return None;
                }

                shell_path.push(file_name);
            }
            _ => return None,
        }
    }

    if shell_path.as_os_str().encode_wide().count() + 1 > LEGACY_PATH_CODE_UNIT_LIMIT_WITH_NUL {
        return None;
    }

    Some(shell_path)
}

fn is_legacy_file_name(file_name: &OsStr) -> bool {
    const LEGACY_FILE_NAME_CODE_UNIT_LIMIT: usize = 255;

    let Some(file_name_text) = file_name.to_str() else {
        return false;
    };

    if file_name_text.is_empty()
        || file_name_text.ends_with(' ')
        || file_name_text.ends_with('.')
        || OsStr::new(file_name_text).encode_wide().count() > LEGACY_FILE_NAME_CODE_UNIT_LIMIT
        || is_reserved_device_name(file_name_text)
    {
        return false;
    }

    !file_name_text.chars().any(|character| {
        matches!(
            character,
            '\0'..='\u{1f}' | '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        )
    })
}

fn is_reserved_device_name(file_name: &str) -> bool {
    const RESERVED_NAMES: [&str; 28] = [
        "AUX", "CON", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9", "LPT¹", "LPT²", "LPT³", "NUL", "PRN",
    ];

    let stem = file_name
        .split('.')
        .next()
        .unwrap_or(file_name)
        .trim_end_matches(' ');

    RESERVED_NAMES
        .iter()
        .any(|reserved_name| stem.eq_ignore_ascii_case(reserved_name))
}
