use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    time::{Duration, SystemTime},
};
use windows::Win32::{
    Foundation::{E_FAIL, FILETIME, PROPERTYKEY},
    System::Com,
    UI::Shell::{IShellItem, IShellItem2, SIGDN},
};
use windows_core::PWSTR;

const FILE_TIME_TICKS_PER_SECOND: u64 = 10_000_000;
const NANOSECONDS_PER_FILE_TIME_TICK: u64 = 100;
const UNIX_EPOCH_FILE_TIME_TICKS: u64 = 116_444_736_000_000_000;

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
