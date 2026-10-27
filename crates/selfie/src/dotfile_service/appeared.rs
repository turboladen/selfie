//! Settling a target that something appeared at while it was being written, which
//! the write left in place. One policy for both content classes: this run writes
//! nothing over what appeared, and leaves any prompt or copy to the next run.

use std::future::Future;

use crate::{
    fs::target::TargetPath,
    package::event::{EntryOperation, EventSender, Failure},
};

/// The window in which something appeared at a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum During {
    /// It appeared after the last read found the target absent, and the write
    /// kept it.
    Write,
}

/// One entry's side of settling a target that appeared: how it reads the target
/// again, and what each settlement does on its path.
pub(super) trait Appeared {
    /// What settling the entry ends in.
    type Outcome;

    /// The bytes the entry was writing.
    fn content(&self) -> &[u8];

    /// The target, as the sentences name it.
    fn target(&self) -> &TargetPath;

    fn sender(&self) -> &EventSender;

    /// Read the target again, with every guard a read takes: its bytes, `None` for
    /// nothing there, or the outcome of anything the read settled itself, already
    /// reported.
    fn reread(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Outcome>> + Send;

    /// Report the entry as failed with `failure`.
    fn failed(&mut self, failure: Failure) -> impl Future<Output = Self::Outcome> + Send;

    /// Settle a target that already holds the content, writing nothing over it.
    fn in_sync(&mut self) -> impl Future<Output = Self::Outcome> + Send;

    /// Report a conflict between the content and `current`, the target's bytes.
    fn conflict(&mut self, current: &[u8]) -> impl Future<Output = Self::Outcome> + Send;
}

/// Settle `entry`, whose write left in place something that appeared at its
/// target: in sync when it holds the content, a conflict when it differs, and a
/// failure when it is gone again. Never puts the conflict to a resolver or to
/// `--yes`, since nobody has seen what appeared.
// What appeared is treated as untracked, whatever the deploy state says about the
// target: the same bytes are in sync and anything else is a conflict, which is
// `deploy_decision`'s answer for an untracked target, reported here, not resolved.
pub(super) async fn settle<A: Appeared + Send>(entry: &mut A) -> A::Outcome {
    let current = match entry.reread().await {
        Ok(current) => current,
        Err(outcome) => return outcome,
    };
    match current {
        None => {
            let failure = gone_again(entry.target());
            entry.failed(failure).await
        }
        Some(current) => settle_present(entry, &current, During::Write).await,
    }
}

/// Settle `entry`, whose target holds `current`, which nobody has seen and which
/// got there `during` that window: in sync when it is the content, else a conflict
/// that is reported, not resolved. Writes nothing over it.
pub(super) async fn settle_present<A: Appeared + Send>(
    entry: &mut A,
    current: &[u8],
    during: During,
) -> A::Outcome {
    if current == entry.content() {
        return entry.in_sync().await;
    }
    let outcome = entry.conflict(current).await;
    // After the conflict, so it reads as a note on it.
    let warning = warning(entry.target(), during);
    entry.sender().send_warning(warning).await;
    outcome
}

/// The warning that what appeared at `target` during `during` was left for the
/// next run. Names the path only, so it is safe for a secret target.
pub(super) fn warning(target: &TargetPath, during: During) -> String {
    let target = target.display();
    let what = match during {
        During::Write => format!("Something appeared at '{target}' while selfie was writing it"),
    };
    format!("{what}, and was left in place. Run apply again to settle it.")
}

/// The failure of an entry whose target something appeared at during the write
/// and then left again.
fn gone_again(target: &TargetPath) -> Failure {
    Failure::new(
        EntryOperation::Write,
        format_args!("Failed to write '{}'", target.display()),
        "something appeared there while selfie was writing it, and is gone again; nothing was \
         written",
    )
    .with_remedy("Run apply again.")
}
