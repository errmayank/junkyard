use std::path::{Path, PathBuf};

use crate::{Result, Trash, TrashItem};

pub(crate) fn discard(_: &Trash, _: &Path) -> Result<TrashItem> {
    unimplemented!()
}

pub(crate) fn discard_all(_: &Trash, _: &[PathBuf]) -> Result<Vec<TrashItem>> {
    unimplemented!()
}
