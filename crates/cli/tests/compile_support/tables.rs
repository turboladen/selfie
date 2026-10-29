// Stands in for the CLI's tables module, which terminal_text.rs names when it
// marks a rendered table as Trusted.
pub(crate) struct SafeTable;

impl std::fmt::Display for SafeTable {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}
