---
paths:
  - "crates/**/*.rs"
---

# Testing

## A green test tells you nothing about whether it can fail

Four tests in this codebase asserted a property they could not observe being violated. Each was
written from the same mental model as the code, so it inherited the code's blind spot:

- Two entries sharing a binding map — both used the **same var name**, so a shared map and a fresh
  one are indistinguishable.
- `stop_on_error` discarding deploy state — the package had **one entry**, so nothing had been
  deployed before the failure.
- Permissions tightening — the target was seeded with **different content**, so the test took the
  conflict path and never reached the skip it was written for.
- Owner-only checking — all three fixtures were `0644`, so a check ignoring **group bits entirely**
  passed every one.

None was found by reading. All were found by mutation.

## Mutation practice

For any test asserting a security or correctness invariant, **write the mutation that violates the
invariant and confirm the named test fails.** Then revert. Record the result.

- **Vary the fixture along the axis under test.** Same-name bindings cannot detect a shared map;
  same-mode fixtures cannot detect an ignored permission bit.
- **Mutations that pass are informative.** Two are worth recording for what they _don't_ do: a
  derived `Debug` on a secret type fails no test (nothing formats it — the case for removing the
  exit by construction), and a `tracing::debug!` of secret content fails only the tracing test and
  not the event tests (the proof they cover different egress rather than duplicating each other).
- **Add a control.** If a test asserts a resolver received a value, mutate it to hand over empty
  slices and confirm the assertion fails — otherwise it may be passing vacuously.
- **A compile error beats a test.** Where a property admits it, prove it by making the violation
  fail to build — adding a hypothetical enum variant, or a boundary that drops a field so a leak
  path cannot be written. A build failure cannot be skipped, ignored, or deleted by someone who does
  not understand what it was for.

## The harness is an instrument and needs verifying too

A batched mutation runner that writes a file and invokes `cargo` within the same second can hit
cargo's mtime staleness check and test a **stale binary**, silently reporting the wrong answer in
either direction. A false "no failure" wastes an investigation; a false "bites" lets a real gap
through and nobody learns.

Run one mutation per invocation, in a `git archive` copy with its own `CARGO_TARGET_DIR`, assert the
mutation's anchor before substituting, distinguish a rustc error from a test failure, and assert
cargo actually built the crate — `Compiling selfie v` **or** `Checking selfie v`. A mutation that
does not compile must be reported as never having run, not counted as caught.

Match **both** verbs, not just the first: `cargo test` prints `Compiling`, but `cargo clippy` and
`cargo check` print `Checking`. A detector looking only for `Compiling` reports every
clippy-verified mutation as never having run — which this file's own wording caused, on a run whose
clippy output contained a real gate failure. Do not "simplify" it back to one verb.

That compile line is **necessary but not sufficient**: cargo prints it when it starts the crate, so
it appears above `error: could not compile` on a build that then fails. Check `could not compile`
(and `error[E`) **first**, and only treat the build as having happened if neither is present —
otherwise a mutation that never built is scored **caught**, because the expected test did not pass
and the compile line was there. A false "caught" is the worse of the two: it records a real coverage
gap as covered, and nobody looks again.

**An absent diagnostic is evidence only if every earlier compiler pass succeeded.** rustc stops
before privacy checking when type checking fails, so a `field is private` error that a clean tree
reports is **absent entirely** — not reordered, not truncated — from a tree that has two unrelated
type errors in it. Anyone verifying "rustc does not complain about X" mid-refactor gets the opposite
of the truth. This is not specific to privacy: it holds for any post-typeck diagnostic. Confirm the
tree is otherwise clean before recording a negative.

A substitution the compiler can discard has not run either — one that binds a value nothing reads,
or that adds a call beside the original instead of replacing it, compiles clean and changes nothing,
so every check passes and it is scored **survived**. Confirm the mutated line is reachable and its
value observed before recording either verdict: the two failure modes are symmetric, and neither
shows up in an exit status.

## A test that watches control flow cannot see what the program said

Three refusal messages shipped wrong in one change: one claimed a supported feature was unsupported,
one described dotfiles on a command that writes none, and one named root on a path that refuses
non-root users too. Every test passed, and each was right about what it asserted — that the refusal
fired, and which variant it was.

Assert the _content_ of anything a user reads, and prefer asserting what it must **not** say: the
wording stays editable, while a copy-paste between two arms fails. Check every method that
contributes, not the first one — the third defect above survived in `suggestion()` after `message()`
was fixed and tested.

**Run the binary and read the output.** All three were found that way, twice from a container run,
never from the suite. Behavior that depends on process identity — euid, `SUDO_UID` — cannot be
exercised from `cargo test` at all: mock the port for the rule, then confirm the real thing in a
throwaway container (`docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/build rust:latest`
— a separate target dir because macOS artifacts are Mach-O).

## Ordering

Do not depend on filesystem enumeration order. `readdir` returns sorted-ish order on macOS/APFS and
hash order on Linux ext4, so an ordering assumption passes locally and fails in CI.
