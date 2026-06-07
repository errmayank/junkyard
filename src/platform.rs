#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::{discard, discard_all};
#[cfg(target_os = "macos")]
pub(crate) use macos::{discard, discard_all};
#[cfg(target_os = "windows")]
pub(crate) use windows::{discard, discard_all};
