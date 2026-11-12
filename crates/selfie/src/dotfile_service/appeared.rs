//! Settling a target that holds bytes nobody has seen: something that appeared
//! while the target was being written, which the write left in place, or a secret
//! target that changed while its conflict prompt was open. One policy for both
//! content classes: this run writes nothing over it, and leaves any prompt or copy
//! to the next run.

use std::future::Future;

use crate::{
    fs::target::TargetPath,
    package::event::{EntryOperation, EventSender, Failure},
    parts::{Message, Parts},
};

/// The window in which something appeared at a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum During {
    /// It appeared after the last read found the target absent, and the write
    /// kept it.
    Write,
    /// It was put there while a conflict prompt about the target's earlier bytes
    /// was open.
    Prompt,
    /// It replaced the bytes a rewrite of the target last read, and the rewrite
    /// kept it.
    Rewrite,
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

    /// The window in which the target changed.
    fn during(&self) -> During;

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
/// target during [`Appeared::during`]: in sync when it holds the content, a conflict when it
/// differs, and a failure when it is gone. Never puts the conflict to a resolver or to
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
            let failure = gone_again(entry.target(), entry.during());
            entry.failed(failure).await
        }
        Some(current) => settle_present(entry, &current).await,
    }
}

/// Settle `entry`, whose target holds `current`, which nobody has seen and which
/// got there during [`Appeared::during`]: in sync when it is the content, else a conflict
/// that is reported, not resolved. Writes nothing over it, and never puts the
/// conflict to a resolver or to `--yes`.
pub(super) async fn settle_present<A: Appeared + Send>(
    entry: &mut A,
    current: &[u8],
) -> A::Outcome {
    if current == entry.content() {
        return entry.in_sync().await;
    }
    let outcome = entry.conflict(current).await;
    // After the conflict, so it reads as a note on it.
    let warning = warning(entry.target(), entry.during());
    entry.sender().send_composed_warning(warning).await;
    outcome
}

/// The warning that what appeared at `target` during `during` was left for the
/// next run. Names the path only, so it is safe for a secret target.
pub(super) fn warning(target: &TargetPath, during: During) -> Message {
    let what = match during {
        During::Write => Parts::new()
            .text("Something appeared at '")
            .path(target.path())
            .text("' while selfie was writing it"),
        During::Prompt => Parts::new()
            .text("'")
            .path(target.path())
            .text("' changed while selfie was asking about it"),
        During::Rewrite => Parts::new()
            .text("'")
            .path(target.path())
            .text("' changed while selfie was writing it"),
    };
    what.text(", and was left in place. Run apply again to settle it.")
        .into()
}

/// The failure of an entry whose target was gone when read again after
/// `during`.
fn gone_again(target: &TargetPath, during: During) -> Failure {
    let cause = match during {
        During::Write => {
            "something appeared there while selfie was writing it, and is gone again; nothing \
             was written"
        }
        During::Prompt => "it was removed while selfie was asking about it; nothing was written",
        During::Rewrite => "it was removed while selfie was writing it; nothing was written",
    };
    Failure::new(
        EntryOperation::Write,
        Parts::new()
            .text("Failed to write '")
            .path(target.path())
            .text("'"),
        Parts::new().text(cause),
    )
    .with_remedy(Parts::new().text("Run apply again."))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::target::repository_path;

    // What appeared and left again fails the write in these words, whole.
    #[test]
    fn a_target_gone_again_is_worded_whole() {
        let failure = gone_again(&repository_path(Path::new("/home/u/my rc")), During::Write);

        assert_eq!(
            failure.message.to_string(),
            "Failed to write '/home/u/my rc': something appeared there while selfie was writing \
             it, and is gone again; nothing was written. Run apply again."
        );
        assert_eq!(
            failure.error.to_string(),
            "something appeared there while selfie was writing it, and is gone again; nothing \
             was written"
        );
    }

    // A rewritten target that was removed before the rewrite landed fails it in
    // these words, whole: nothing appeared there.
    #[test]
    fn a_rewritten_target_gone_is_worded_whole() {
        let failure = gone_again(
            &repository_path(Path::new("/home/u/my rc")),
            During::Rewrite,
        );

        assert_eq!(
            failure.message.to_string(),
            "Failed to write '/home/u/my rc': it was removed while selfie was writing it; \
             nothing was written. Run apply again."
        );
    }

    // What appeared during each window is reported in these words, whole.
    #[test]
    fn a_target_that_appeared_is_worded_whole() {
        let target = repository_path(Path::new("/home/u/my rc"));

        assert_eq!(
            warning(&target, During::Write).to_string(),
            "Something appeared at '/home/u/my rc' while selfie was writing it, and was left in \
             place. Run apply again to settle it."
        );
        assert_eq!(
            warning(&target, During::Prompt).to_string(),
            "'/home/u/my rc' changed while selfie was asking about it, and was left in place. Run \
             apply again to settle it."
        );
        assert_eq!(
            warning(&target, During::Rewrite).to_string(),
            "'/home/u/my rc' changed while selfie was writing it, and was left in place. Run apply \
             again to settle it."
        );
    }

    // The target is a path part in both windows.
    #[test]
    fn a_target_that_appeared_is_a_path_part() {
        let target = repository_path(Path::new("/h/r\u{200B}c"));

        for during in [During::Write, During::Prompt, During::Rewrite] {
            assert_eq!(
                crate::parts::values_of(&warning(&target, during)),
                ["/h/r\u{200B}c"]
            );
        }
    }
}
