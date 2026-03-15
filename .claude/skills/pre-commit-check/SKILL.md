---
name: pre-commit-check
description: Run the full pre-commit checklist (cargo fmt, clippy zero-warnings, cargo test) and report results before committing
---

Run the project's pre-commit checklist. Execute these steps sequentially and stop on the first
failure:

1. **Format**: `cargo fmt` — auto-fix formatting
2. **Lint**: `cargo clippy --all-targets` — zero warnings policy; fix any warnings found
3. **Test**: `cargo test` — all tests must pass

Report a summary of results. If any step fails, show the failure details and do not proceed to later
steps.
