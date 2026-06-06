mod path;
mod recycle;
mod shell;

use std::path::{Path, PathBuf};

use recycle::{RecycleOperation, RecycleProgressSink, RecycledItem};
use shell::{ShellContext, with_shell_context};

use crate::{Error, Result, Trash, TrashItem};

pub(crate) fn discard(_: &Trash, path: &Path) -> Result<TrashItem> {
    let path = path.to_path_buf();

    with_shell_context(move |shell_context| discard_inner(shell_context, &path))
}

pub(crate) fn discard_all(_: &Trash, paths: &[PathBuf]) -> Result<Vec<TrashItem>> {
    let paths = paths.to_vec();

    with_shell_context(move |shell_context| {
        paths
            .iter()
            .map(|path| discard_inner(shell_context, path))
            .collect()
    })
}

fn discard_inner(shell_context: &ShellContext, path: &Path) -> Result<TrashItem> {
    if path.file_name().is_none() {
        return Err(Error::TargetedRoot {
            path: path.to_path_buf(),
        });
    }

    let shell_item = shell_context.item_from_path(path)?;
    let progress_sink = RecycleProgressSink::new();
    let file_operation_progress_sink = progress_sink.to_file_operation_progress_sink();
    let operation = RecycleOperation::new(shell_context, path)?;

    operation.queue_delete(path, &shell_item, &file_operation_progress_sink)?;
    operation.execute(path, &progress_sink)?;

    let recycled_item = RecycledItem::from_progress(shell_context, &progress_sink, path)?;

    Ok(TrashItem::new(
        recycled_item.id,
        recycled_item.original_name,
        recycled_item.original_parent,
        recycled_item.discarded_at,
    ))
}
