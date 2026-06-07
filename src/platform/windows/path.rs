use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt};

pub(super) trait ShellOsStrExt {
    fn wide_path(&self) -> Result<Vec<u16>, io::Error>;
}

impl ShellOsStrExt for OsStr {
    fn wide_path(&self) -> Result<Vec<u16>, io::Error> {
        let mut wide_path = self.encode_wide().collect::<Vec<_>>();

        if wide_path.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "string contains an interior NUL",
            ));
        }

        wide_path.push(0);

        Ok(wide_path)
    }
}
