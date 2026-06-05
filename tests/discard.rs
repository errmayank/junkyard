#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix;
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::{
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
};
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::GetLongPathNameW;
#[cfg(target_os = "windows")]
use windows_core::PCWSTR;

use tempfile::TempDir;

use junkyard::{Error, Trash};

#[cfg(target_os = "windows")]
fn to_long_path(path: &Path) -> io::Result<PathBuf> {
    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();

    if wide_path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }

    wide_path.push(0);

    // SAFETY: `wide_path` is NUL-terminated and remains valid for the duration of this call.
    let buffer_code_units = unsafe { GetLongPathNameW(PCWSTR(wide_path.as_ptr()), None) };
    if buffer_code_units == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut long_path = vec![0; buffer_code_units as usize];

    // SAFETY: `wide_path` is NUL-terminated. `long_path` is a writable output buffer for this call.
    let copied_code_units =
        unsafe { GetLongPathNameW(PCWSTR(wide_path.as_ptr()), Some(&mut long_path)) };
    if copied_code_units == 0 {
        return Err(io::Error::last_os_error());
    }

    long_path.truncate(copied_code_units as usize);

    Ok(PathBuf::from(OsString::from_wide(&long_path)))
}

#[test]
fn test_discard_file() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let file = temp_dir.path().join("file.txt");

    std::fs::write(&file, b"junk").unwrap();

    #[cfg(target_os = "windows")]
    let expected_path = to_long_path(&file).unwrap();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let expected_path = file.canonicalize().unwrap();

    let trashed_item = trash.discard(&file).unwrap();

    assert_eq!(trashed_item.original_name(), file.file_name().unwrap());
    assert_eq!(trashed_item.original_path(), expected_path);
    assert!(!file.exists());
}

#[test]
fn test_discard_file_name_with_special_chars() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let file_name = if cfg!(target_os = "windows") {
        "percent% plus+ comma, café 日本語.txt"
    } else {
        r#"quote" percent% plus+ comma, backslash\ café 日本語.txt"#
    };
    let file = temp_dir.path().join(file_name);

    std::fs::write(&file, b"junk").unwrap();

    #[cfg(target_os = "windows")]
    let expected_path = to_long_path(&file).unwrap();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let expected_path = file.canonicalize().unwrap();

    let trashed_item = trash.discard(&file).unwrap();

    assert_eq!(trashed_item.original_name(), file.file_name().unwrap());
    assert_eq!(trashed_item.original_path(), expected_path);
    assert!(!file.exists());
}

#[test]
fn test_discard_files_with_same_name() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let first_dir = temp_dir.path().join("first");
    let second_dir = temp_dir.path().join("second");
    let first = first_dir.join("file.txt");
    let second = second_dir.join("file.txt");

    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();

    let first_item = trash.discard(&first).unwrap();
    let second_item = trash.discard(&second).unwrap();

    assert_eq!(first_item.original_name(), first.file_name().unwrap());
    assert_eq!(second_item.original_name(), second.file_name().unwrap());
    assert_ne!(first_item.id(), second_item.id());
    assert!(!first.exists());
    assert!(!second.exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn test_discard_file_with_parent_symlink() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let dir = temp_dir.path().join("directory");
    let dir_link = temp_dir.path().join("directory-link");
    let file = temp_dir.path().join("file.txt");
    let file_link = dir.join("file-link.txt");

    std::fs::create_dir(&dir).unwrap();
    std::fs::write(&file, b"content").unwrap();
    unix::fs::symlink(&dir, &dir_link).unwrap();
    unix::fs::symlink(&file, &file_link).unwrap();

    let expected_parent = dir.canonicalize().unwrap();
    let trashed_item = trash.discard(dir_link.join("file-link.txt")).unwrap();

    assert_eq!(trashed_item.original_parent(), expected_parent);
    assert_eq!(
        trashed_item.original_path(),
        expected_parent.join("file-link.txt")
    );
    assert!(!file_link.exists());
    assert!(file.exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn test_discard_broken_symlink() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let missing_target = temp_dir.path().join("missing.txt");
    let file_link = temp_dir.path().join("file-link.txt");

    unix::fs::symlink(&missing_target, &file_link).unwrap();

    let trashed_item = trash.discard(&file_link).unwrap();

    assert_eq!(trashed_item.original_name(), file_link.file_name().unwrap());
    assert_eq!(
        trashed_item.original_path(),
        temp_dir
            .path()
            .canonicalize()
            .unwrap()
            .join("file-link.txt")
    );
    let error = std::fs::symlink_metadata(&file_link).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(!missing_target.exists());
}

#[test]
fn test_discard_directory() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let dir = temp_dir.path().join("directory");
    let file = dir.join("file.txt");

    std::fs::create_dir(&dir).unwrap();
    std::fs::write(file, b"junk").unwrap();

    #[cfg(target_os = "windows")]
    let expected_path = to_long_path(&dir).unwrap();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let expected_path = dir.canonicalize().unwrap();

    let trashed_item = trash.discard(&dir).unwrap();

    assert_eq!(trashed_item.original_name(), dir.file_name().unwrap());
    assert_eq!(trashed_item.original_path(), expected_path);
    assert!(!dir.exists());
}

#[test]
fn test_discard_all() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let first = temp_dir.path().join("first.txt");
    let second = temp_dir.path().join("second.txt");
    let third = temp_dir.path().join("third.txt");
    let dir = temp_dir.path().join("directory");
    let fourth = dir.join("fourth.txt");

    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    std::fs::write(&third, b"third").unwrap();
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(&fourth, b"fourth").unwrap();

    let trashed_items = trash.discard_all([&first, &second, &dir]).unwrap();

    assert_eq!(trashed_items.len(), 3);
    assert!(!first.exists());
    assert!(!second.exists());
    assert!(third.exists());
    assert!(!dir.exists());
}

#[test]
fn test_discard_empty_path() {
    let trash = Trash::new();
    let result = trash.discard(Path::new(""));

    assert!(matches!(result, Err(Error::EmptyPath)));
}

#[test]
fn test_discard_all_with_invalid_path_aborts() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let first = temp_dir.path().join("first.txt");
    let second = temp_dir.path().join("second.txt");

    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();

    let result = trash.discard_all([first.as_path(), Path::new(""), second.as_path()]);

    assert!(matches!(result, Err(Error::EmptyPath)));
    assert!(first.exists());
    assert!(second.exists());

    let root = std::env::current_dir()
        .unwrap()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();
    let result = trash.discard_all([first.as_path(), root.as_path(), second.as_path()]);

    assert!(matches!(result, Err(Error::TargetedRoot { .. })));
    assert!(first.exists());
    assert!(second.exists());
}

#[test]
fn test_discard_root_path() {
    let trash = Trash::new();
    let root = std::env::current_dir()
        .unwrap()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();
    let result = trash.discard(root);

    assert!(matches!(result, Err(Error::TargetedRoot { .. })));
}

#[test]
fn test_temp() {
    let temp_dir = TempDir::new().unwrap();
    let trash = Trash::new();
    let file = temp_dir.path().join("temp.txt");

    std::fs::write(&file, b"junk").unwrap();

    let trashed_item = trash.discard(&file).unwrap();

    eprintln!("Temporary trash item diagnostics:");
    eprintln!("id debug: {:?}", trashed_item.id());
    eprintln!("id lossy: {}", trashed_item.id().to_string_lossy());
    eprintln!("original_name debug: {:?}", trashed_item.original_name());
    eprintln!(
        "original_parent debug: {:?}",
        trashed_item.original_parent()
    );
    eprintln!("original_path debug: {:?}", trashed_item.original_path());
    eprintln!("discarded_at debug: {:?}", trashed_item.discarded_at());

    assert_eq!(trashed_item.id(), Path::new("temp").as_os_str());
}
