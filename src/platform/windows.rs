mod path;
mod recycle;
mod shell;

use recycle::{RecycleOperation, RecycleProgressSink, RecycledItem};
use shell::{ShellContext, with_shell_context};

use crate::{Result, TrashItem, discard::DiscardTarget};

pub(crate) fn discard(target: &DiscardTarget) -> Result<TrashItem> {
    let target = target.clone();

    with_shell_context(move |shell_context| discard_inner(shell_context, &target))
}

pub(crate) fn discard_all(targets: &[DiscardTarget]) -> Result<Vec<TrashItem>> {
    let targets = targets.to_vec();

    with_shell_context(move |shell_context| {
        targets
            .iter()
            .map(|target| discard_inner(shell_context, target))
            .collect()
    })
}

pub(crate) fn restore(_item: TrashItem) -> Result<()> {
    unimplemented!()
}

pub(crate) fn restore_all(_items: Vec<TrashItem>) -> Result<()> {
    unimplemented!()
}

fn discard_inner(shell_context: &ShellContext, target: &DiscardTarget) -> Result<TrashItem> {
    let path = &target.path;
    let shell_item = shell_context.item_from_path(path)?;
    let sink = RecycleProgressSink::new();
    let file_operation_sink = sink.to_file_operation_sink();
    let operation = RecycleOperation::new(shell_context, path)?;

    operation.queue_delete(path, &shell_item, &file_operation_sink)?;
    operation.execute(path, &sink)?;

    let recycled_item = RecycledItem::from_progress(shell_context, &sink, path)?;

    Ok(TrashItem::new(
        recycled_item.id,
        recycled_item.original_name,
        recycled_item.original_parent,
        recycled_item.discarded_at,
    ))
}
