use std::{
    ffi::{OsStr, OsString},
    marker::PhantomData,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    rc::Rc,
    thread::Builder,
    time::{Duration, SystemTime},
};
use windows::Win32::{
    Foundation::{E_FAIL, FILETIME, PROPERTYKEY},
    System::Com::{self, CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
    UI::Shell::{self, FileOperation, IFileOperation, IShellItem, IShellItem2, SIGDN},
};
use windows_core::{GUID, Interface, PCWSTR, PWSTR};

use super::path::ShellOsStrExt;
use crate::{Error, Result};

const FILE_TIME_TICKS_PER_SECOND: u64 = 10_000_000;
const MAX_UNPREFIXED_SHELL_PATH_CODE_UNITS_WITH_NUL: usize = 260;
const MAX_SHELL_FILE_NAME_CODE_UNITS: usize = 255;
const NANOSECONDS_PER_FILE_TIME_TICK: u64 = 100;
const UNIX_EPOCH_FILE_TIME_TICKS: u64 = 116_444_736_000_000_000;

#[derive(Debug)]
struct ComApartment {
    _thread_bound: PhantomData<Rc<()>>,
}

impl ComApartment {
    fn new() -> Result<Self> {
        // SAFETY: `CoInitializeEx` initializes COM for the current thread with valid
        // flags. `ComApartment` calls `CoUninitialize` after a successful initialization.
        let result =
            unsafe { Com::CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };

        result.ok().map_err(|source| Error::Platform {
            message: format!("Failed to initialize Windows COM apartment: {source}"),
        })?;

        Ok(Self {
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: `ComApartment` is constructed after `CoInitializeEx` succeeds
        // and is dropped on the same worker thread that initialized COM.
        unsafe {
            Com::CoUninitialize();
        }
    }
}

#[derive(Debug)]
pub(super) struct ShellContext {
    _com_apartment: ComApartment,
}

impl ShellContext {
    fn new() -> Result<Self> {
        Ok(Self {
            _com_apartment: ComApartment::new()?,
        })
    }

    pub(super) fn item_from_path(&self, path: &Path) -> Result<IShellItem> {
        if let Some(compatible_path) = shell_path(path) {
            let compatible_wide_path =
                compatible_path
                    .as_os_str()
                    .wide_path()
                    .map_err(|source| Error::Io {
                        path: compatible_path.clone(),
                        source,
                    })?;

            match self.item_from_wide_path::<IShellItem>(&compatible_wide_path) {
                Ok(shell_item) => Ok(shell_item),
                Err(shell_path_error) => {
                    let original_wide_path =
                        path.as_os_str().wide_path().map_err(|source| Error::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;

                    self.item_from_wide_path::<IShellItem>(&original_wide_path)
                        .map_err(|path_error| Error::Platform {
                            message: format!(
                                "Failed to create Windows shell item for {}: {shell_path_error}; {path_error}",
                                path.display()
                            ),
                        })
                }
            }
        } else {
            let original_wide_path = path.as_os_str().wide_path().map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;

            self.item_from_wide_path::<IShellItem>(&original_wide_path)
                .map_err(|path_error| Error::Platform {
                    message: format!(
                        "Failed to create Windows shell item for {}: {path_error}",
                        path.display()
                    ),
                })
        }
    }

    pub(super) fn item_from_wide_path<T>(&self, wide_path: &[u16]) -> windows_core::Result<T>
    where
        T: Interface,
    {
        let Some((terminator, path_without_nul)) = wide_path.split_last() else {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell parsing name was empty",
            ));
        };
        if *terminator != 0 {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell parsing name was not NUL-terminated",
            ));
        }
        if path_without_nul.contains(&0) {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell parsing name contained an interior NUL",
            ));
        }

        // SAFETY: `ShellContext` guarantees COM is initialized on the current thread.
        // The checks above ensure `wide_path` is valid for this call.
        unsafe { Shell::SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None) }
    }

    pub(super) fn file_operation(&self, path: &Path) -> Result<IFileOperation> {
        // SAFETY: `ShellContext` guarantees COM is initialized on the current thread.
        // `FileOperation` is the Shell-provided CLSID for `IFileOperation`.
        let operation = unsafe {
            Com::CoCreateInstance(std::ptr::from_ref::<GUID>(&FileOperation), None, CLSCTX_ALL)
        }
        .map_err(|source| Error::Platform {
            message: format!(
                "Failed to create Windows file operation for {}: {source}",
                path.display()
            ),
        })?;

        Ok(operation)
    }
}

pub(super) fn with_shell_context<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&ShellContext) -> Result<T> + Send + 'static,
{
    let thread = Builder::new()
        .name("junkyard-discard".to_owned())
        .spawn(move || {
            let shell_context = ShellContext::new()?;

            operation(&shell_context)
        })
        .map_err(|source| Error::Platform {
            message: format!("Failed to spawn Windows trash thread: {source}"),
        })?;

    match thread.join() {
        Ok(result) => result,
        Err(_) => Err(Error::Platform {
            message: "Windows trash thread panicked".to_owned(),
        }),
    }
}

fn shell_path(path: &Path) -> Option<PathBuf> {
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

    if shell_path.as_os_str().encode_wide().count() + 1
        > MAX_UNPREFIXED_SHELL_PATH_CODE_UNITS_WITH_NUL
    {
        return None;
    }

    Some(shell_path)
}

fn is_legacy_file_name(file_name: &OsStr) -> bool {
    let Some(file_name_text) = file_name.to_str() else {
        return false;
    };

    if file_name_text.is_empty()
        || file_name_text.ends_with(' ')
        || file_name_text.ends_with('.')
        || OsStr::new(file_name_text).encode_wide().count() > MAX_SHELL_FILE_NAME_CODE_UNITS
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

#[derive(Debug)]
pub(super) struct ShellString(PWSTR);

impl ShellString {
    pub(super) fn from_display_name(
        shell_item: &IShellItem,
        kind: SIGDN,
    ) -> windows_core::Result<Self> {
        // SAFETY: `shell_item` remains valid for the duration of this call and the
        // returned string pointer is released by `ShellString::drop`.
        let pointer = unsafe { shell_item.GetDisplayName(kind)? };
        if pointer.is_null() {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell returned a null display name for the recycled item",
            ));
        }

        Ok(Self(pointer))
    }

    pub(super) fn from_property_string(
        shell_item: &IShellItem2,
        key: &PROPERTYKEY,
    ) -> windows_core::Result<Self> {
        // SAFETY: `shell_item` remains valid for the duration of this call and the
        // returned string pointer is released by `ShellString::drop`.
        let pointer = unsafe { shell_item.GetString(key)? };
        if pointer.is_null() {
            return Err(windows_core::Error::new(
                E_FAIL,
                "Shell returned a null string property for the recycled item",
            ));
        }

        Ok(Self(pointer))
    }

    pub(super) fn into_os_string(self) -> OsString {
        // SAFETY: `ShellString` is constructed from a non-null
        // Shell string pointer, which is a NUL-terminated UTF-16 string.
        let display_name_wide = unsafe { self.0.as_wide() };

        OsString::from_wide(display_name_wide)
    }
}

impl Drop for ShellString {
    fn drop(&mut self) {
        // SAFETY: `ShellString` stores a Shell-allocated string pointer
        // and this is the only place that releases it.
        unsafe {
            Com::CoTaskMemFree(Some(self.0.as_ptr().cast::<std::ffi::c_void>()));
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct FileTime(u64);

impl FileTime {
    const UNIX_EPOCH: Self = Self(UNIX_EPOCH_FILE_TIME_TICKS);

    pub(super) fn from_windows(file_time: FILETIME) -> Self {
        Self((u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime))
    }

    pub(super) fn to_system_time(self) -> windows_core::Result<SystemTime> {
        let ticks_from_unix_epoch = self.0.abs_diff(Self::UNIX_EPOCH.0);
        let seconds = ticks_from_unix_epoch / FILE_TIME_TICKS_PER_SECOND;
        let nanoseconds = u32::try_from(
            (ticks_from_unix_epoch % FILE_TIME_TICKS_PER_SECOND) * NANOSECONDS_PER_FILE_TIME_TICK,
        )
        .map_err(|source| {
            windows_core::Error::new(
                E_FAIL,
                format!("Shell returned an invalid recycled item deletion date: {source}"),
            )
        })?;

        let duration = Duration::new(seconds, nanoseconds);
        let system_time = if self.0 >= Self::UNIX_EPOCH.0 {
            SystemTime::UNIX_EPOCH.checked_add(duration)
        } else {
            SystemTime::UNIX_EPOCH.checked_sub(duration)
        };

        system_time.ok_or_else(|| {
            windows_core::Error::new(
                E_FAIL,
                "Shell returned a recycled item deletion date outside the supported time range",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_path_converts_verbatim_disk_path() {
        assert_eq!(
            shell_path(Path::new(r"\\?\C:\foo\bar")).as_deref(),
            Some(Path::new(r"C:\foo\bar"))
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\Z:\foo\bar")).as_deref(),
            Some(Path::new(r"Z:\foo\bar"))
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\c:\foo")).as_deref(),
            Some(Path::new(r"c:\foo"))
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\Z:\🧪\📦")).as_deref(),
            Some(Path::new(r"Z:\🧪\📦"))
        );
    }

    #[test]
    fn test_shell_path_rejects_unsafe_verbatim_paths() {
        for path in [
            r"\\?\C:\foo\.\bar",
            r"\\?\C:\foo\..\bar",
            r"\\?\C:\foo/bar",
            r"\\?\c\foo",
            r"\\?\c:foo",
            r"\\?\cc:foo",
            r"\\?\c:foo\bar",
            r"\\?\UNC\server\share\foo",
            r"\\.\C:\notdisk",
            r"\\?\GLOBALROOT\Device\ImDisk0\path\to\file.txt",
        ] {
            assert!(shell_path(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn test_shell_path_rejects_invalid_legacy_file_names() {
        for path in [
            r"\\?\C:\CON",
            r"\\?\C:\COM1.txt",
            r"\\?\C:\nul.tar.gz",
            r"\\?\C:\file.",
            r"\\?\C:\file ",
            r"\\?\C:\foo\bad:name",
            r#"\\?\C:\foo\bad"name"#,
            r"\\?\C:\foo\bad<name",
            r"\\?\C:\foo\bad>name",
            r"\\?\C:\foo\bad|name",
            r"\\?\C:\foo\bad?name",
            r"\\?\C:\foo\bad*name",
            "\\\\?\\C:\\foo\\bad\u{1f}name",
        ] {
            assert!(shell_path(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn test_shell_path_checks_legacy_path_length_with_nul() {
        let file_name = "m".repeat(MAX_SHELL_FILE_NAME_CODE_UNITS - 1);
        let allowed_path = format!(r"\\?\C:\a\{file_name}");
        let expected_path = format!(r"C:\a\{file_name}");

        assert_eq!(
            shell_path(Path::new(&allowed_path)).as_deref(),
            Some(Path::new(&expected_path))
        );

        let too_long_file_name = "m".repeat(MAX_SHELL_FILE_NAME_CODE_UNITS);
        let too_long_path = format!(r"\\?\C:\a\{too_long_file_name}");

        assert!(shell_path(Path::new(&too_long_path)).is_none());
    }

    #[test]
    fn test_file_time_from_windows() {
        let file_time = FileTime::from_windows(FILETIME {
            dwLowDateTime: 0x89ab_cdef,
            dwHighDateTime: 0x0123_4567,
        });

        assert_eq!(file_time.0, 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn test_file_time_converts_nt_epoch() {
        let system_time = FileTime(0).to_system_time().unwrap();

        assert_eq!(
            SystemTime::UNIX_EPOCH.duration_since(system_time).unwrap(),
            Duration::from_secs(UNIX_EPOCH_FILE_TIME_TICKS / FILE_TIME_TICKS_PER_SECOND)
        );
    }

    #[test]
    fn test_file_time_converts_unix_epoch() {
        let system_time = FileTime::UNIX_EPOCH.to_system_time().unwrap();

        assert_eq!(system_time, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn test_file_time_converts_before_and_after_unix_epoch() {
        let before = FileTime(UNIX_EPOCH_FILE_TIME_TICKS - 1)
            .to_system_time()
            .unwrap();
        let after = FileTime(UNIX_EPOCH_FILE_TIME_TICKS + 1)
            .to_system_time()
            .unwrap();

        assert_eq!(
            SystemTime::UNIX_EPOCH.duration_since(before).unwrap(),
            Duration::from_nanos(100)
        );
        assert_eq!(
            after.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            Duration::from_nanos(100)
        );
    }

    #[test]
    fn test_file_time_preserves_subsecond_precision() {
        let system_time = FileTime(UNIX_EPOCH_FILE_TIME_TICKS + FILE_TIME_TICKS_PER_SECOND + 5)
            .to_system_time()
            .unwrap();

        assert_eq!(
            system_time.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            Duration::new(1, 500)
        );
    }
}
