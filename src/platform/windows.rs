mod sta_thread;

use std::{
    ffi::{OsStr, OsString},
    io,
    marker::PhantomData,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use windows::{
    Win32::{
        Foundation::E_FAIL,
        System::Com::{self, CLSCTX_ALL},
        UI::Shell::{
            self, FOF_NO_CONNECTED_ELEMENTS, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
            FOFX_ADDUNDORECORD, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
            IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
            IShellItem, SIGDN_DESKTOPABSOLUTEPARSING,
        },
    },
};
use windows_core::{GUID, HRESULT, PCWSTR, PWSTR, Ref, implement};

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
        let operation: IFileOperation =
            unsafe { Com::CoCreateInstance(&FileOperation as *const GUID, None, CLSCTX_ALL) }
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
            .map(|operations_aborted| operations_aborted.as_bool())
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
                    if let Some(message) =
                        progress_sink.failure_message().map_err(|_| Error::Platform {
                            message: format!(
                                "Failed to perform Windows recycle operation for {}; recycle progress state was poisoned: {source}",
                                path.display()
                            ),
                        })?
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
                if let Some(message) =
                    progress_sink.failure_message().map_err(|_| Error::Platform {
                        message: format!(
                            "Failed to perform Windows recycle operation for {}; recycle progress state was poisoned: {source}",
                            path.display()
                        ),
                    })?
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

    match components.next()? {
        Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::VerbatimDisk(_)) => {}
        _ => return None,
    }

    let shell_path = components.as_path().to_path_buf();
    let mut has_root_directory = false;

    for component in components {
        match component {
            Component::RootDir => has_root_directory = true,
            Component::Normal(file_name) => {
                if !is_legacy_file_name(file_name) {
                    return None;
                }
            }
            _ => return None,
        }
    }

    if !has_root_directory
        || shell_path.as_os_str().encode_wide().count() + 1 > LEGACY_PATH_CODE_UNIT_LIMIT_WITH_NUL
    {
        return None;
    }

    Some(shell_path)
}

fn is_legacy_file_name(file_name: &OsStr) -> bool {
    const LEGACY_FILE_NAME_CODE_UNIT_LIMIT: usize = 255;

    let file_name_text = match file_name.to_str() {
        Some(file_name) => file_name,
        None => return false,
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

fn record_progress_failure(
    state: &Arc<Mutex<RecycleProgressState>>,
    failure: impl Into<String>,
) -> windows_core::Result<()> {
    let mut state = state.lock().map_err(|_| {
        windows_core::Error::new(
            E_FAIL,
            "Recycle progress state was poisoned while recording failure",
        )
    })?;

    *state = RecycleProgressState::Failed {
        message: failure.into(),
    };

    Ok(())
}

#[derive(Debug)]
struct ShellAllocatedString(PWSTR);

impl ShellAllocatedString {
    fn new(shell_item: &IShellItem) -> windows_core::Result<Self> {
        // SAFETY: `shell_item` remains valid for the duration of this call and the
        // returned string pointer is released by `ShellAllocatedString::drop`.
        let pointer = unsafe { shell_item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)? };
        if pointer.is_null() {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell returned a null display name for the recycled item",
            ));
        }

        Ok(Self(pointer))
    }

    fn into_os_string(self) -> OsString {
        // SAFETY: `ShellAllocatedString` is constructed from a non-null
        // `GetDisplayName` pointer, which is a NUL-terminated UTF-16 string.
        let display_name_wide = unsafe { self.0.as_wide() };

        OsString::from_wide(display_name_wide)
    }
}

impl Drop for ShellAllocatedString {
    fn drop(&mut self) {
        // SAFETY: `ShellAllocatedString::new` stores the pointer returned by
        // `GetDisplayName` and this is the only place that releases it.
        unsafe {
            Com::CoTaskMemFree(Some(self.0.as_ptr().cast::<std::ffi::c_void>()));
        }
    }
}

#[derive(Debug, Default)]
enum RecycleProgressState {
    #[default]
    Pending,
    Recycled {
        item_id: OsString,
    },
    Failed {
        message: String,
    },
}

#[implement(IFileOperationProgressSink)]
#[derive(Debug)]
struct RecycleProgressSink {
    state: Arc<Mutex<RecycleProgressState>>,
}

impl RecycleProgressSink {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RecycleProgressState::default())),
        }
    }

    fn to_file_operation_progress_sink(&self) -> IFileOperationProgressSink {
        Self {
            state: Arc::clone(&self.state),
        }
        .into()
    }

    fn failure_message(&self) -> windows_core::Result<Option<String>> {
        let state = self.state.lock().map_err(|_| {
            windows_core::Error::new(
                E_FAIL,
                "Recycle progress state was poisoned while reading failure",
            )
        })?;

        match state.as_ref() {
            RecycleProgressState::Failed { message } => Ok(Some(message.clone())),
            _ => Ok(None),
        }
    }

    fn recycled_item_id(&self, path: &Path) -> Result<OsString> {
        let state = self.state.lock().map_err(|_| Error::Platform {
            message: format!("Recycle progress state was poisoned for {}", path.display()),
        })?;

        match state.as_ref() {
            RecycleProgressState::Pending => Err(Error::Platform {
                message: format!(
                    "Windows did not return a recycled shell item for {}",
                    path.display()
                ),
            }),
            RecycleProgressState::Recycled { item_id } => Ok(item_id.clone()),
            RecycleProgressState::Failed { message } => Err(Error::Platform {
                message: format!("Windows failed to recycle {}: {message}", path.display()),
            }),
        }
    }
}

impl IFileOperationProgressSink_Impl for RecycleProgressSink_Impl {
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        delete_result: HRESULT,
        recycled_item: Ref<'_, IShellItem>,
    ) -> windows_core::Result<()> {
        if delete_result.is_err() {
            let source = windows_core::Error::from(delete_result);
            record_progress_failure(
                &self.state,
                format!("Shell delete operation failed: {source}"),
            )?;

            return Err(source);
        }

        let Some(recycled_item) = recycled_item.as_ref() else {
            let message = "Shell permanently deleted the item instead of recycling it";
            record_progress_failure(&self.state, message)?;

            return Err(windows_core::Error::new(E_FAIL, message));
        };

        let recycled_id = match ShellAllocatedString::new(recycled_item) {
            Ok(display_name) => display_name.into_os_string(),
            Err(source) => {
                record_progress_failure(
                    &self.state,
                    format!("Failed to read recycled shell item name: {source}"),
                )?;

                return Err(source);
            }
        };

        let mut state = self.state.lock().map_err(|_| {
            windows_core::Error::new(
                E_FAIL,
                "Recycle progress state was poisoned while recording recycled item",
            )
        })?;

        *state = RecycleProgressState::Recycled {
            item_id: recycled_id,
        };

        Ok(())
    }

    fn StartOperations(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn FinishOperations(&self, _: HRESULT) -> windows_core::Result<()> {
        Ok(())
    }

    fn PreRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PreDeleteItem(&self, _: u32, _: Ref<'_, IShellItem>) -> windows_core::Result<()> {
        Ok(())
    }

    fn PreNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn UpdateProgress(&self, _: u32, _: u32) -> windows_core::Result<()> {
        Ok(())
    }

    fn ResetTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn PauseTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn ResumeTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
}
