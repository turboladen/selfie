# Comments

## The split

A doc comment answers the **caller**. A body comment answers the **next maintainer**. That is the
whole rule, and getting it wrong is what makes the docs in this repo long.

_"Do not call this before X"_ is the caller's business. _"We did not fsync here because it would
widen the window"_ is the maintainer's.

## What a doc comment contains

Only these, and only where the code does not already say it:

- what it does
- why you should or should not use it
- side effects
- what it returns — often the type already says it
- params — same
- `# Errors`
- `# Panics`

## What it does not contain

Design rationale. Rejected alternatives. What-if scenarios. How it came to be this way. Anything
about its callers.

That material is real and worth keeping. It goes in ordinary `//` comments **in the body**, beside
the code it constrains, where someone about to change that code will see it.

**Relocate, never just delete.** Several blocks here record a trap someone will otherwise
reintroduce — the redact-before-bound order in `git/message.rs`, the `0o300` directory case, why two
guards use different stats. A pass that only shortens loses them.

If the sentence is interesting because of how it was _discovered_, it belongs in the commit message
or a bead. If it is useful because of what it _constrains_, it belongs in the code.

## Traits and impls

A trait's docs state the contract. How one implementation achieves it goes on the impl.

The test: would this sentence still be true if someone wrote a second implementation? If not, it
belongs on the impl.

- **Trait** — what the method answers, what the return values mean, when a caller must call it, and
  how strong the guarantee is, including where it weakens. "On other platforms this is a mitigation,
  not a guarantee" is contract, and a caller needs it.
- **Impl** — which syscall and why, `#[cfg]` behavior, errno handling, platform mechanism.
  `O_NOFOLLOW`, `symlink_metadata` vs `metadata`, `F_FULLFSYNC`.

Keep callers out of trait docs as well. A trait cannot know its callers, and that reasoning already
lives at the call site.

## Prose

**If reading the code is easier than reading the comment, the comment is too long.** Past ~10 lines
a doc comment needs a reason.

Cut on sight:

- stacked em-dash asides
- announcing significance — "That is the point:", "What it must not do is", "load-bearing, not
  incidental"
- "X rather than Y" as a default rhythm
- the same idea restated at three altitudes
- stacked hedges, and sentences carrying three commas and a dash

US spelling, as everywhere else in the repo.

## What may not appear in shipped source

- **`.claude/` paths.** Not in doc comments, not in ordinary comments, not in tests. State the
  constraint directly and drop the pointer. The prohibition runs one way: these rule files may cite
  the codebase freely.
- **Bead IDs.** Ordinary `//` comments only — never `///`, never `//!`, never `docs/`. `cargo doc`
  publishes those to readers who cannot look an ID up.

When a citation is load-bearing, keep the reasoning and lose the reference.

## Where `///` earns its place, and where it does not

`///` desugars to `#[doc]`; `//` desugars to nothing. Under `#![deny(missing_docs)]` a `///`
satisfies the lint and a `//` beside the same item does not. That difference is the whole basis for
what follows — three things read doc attributes, and nothing reads an ordinary comment.

|                                | doctests run | `cargo doc` renders | editor hover |
| ------------------------------ | ------------ | ------------------- | ------------ |
| `pub` item                     | yes          | yes                 | yes          |
| private item, production code  | **yes**      | no                  | yes          |
| anything inside `#[cfg(test)]` | **no**       | no                  | yes          |

Both surprising rows were verified rather than assumed, and each one decides a rule:

- **A private item's doctest runs.** `cargo test --doc` collects it even though `cargo doc` will
  never render the item. So `///` on a private function is a live attribute, not decoration — keep
  it, and know that deleting the marker deletes any test inside it.
- **A doctest inside `#[cfg(test)]` never runs.** rustdoc compiles without `cfg(test)`, so it does
  not see the module at all. There is no doctest to lose and no page to render.

**Nothing private inside `#[cfg(test)]` or under `tests/` gets `///`** — not test functions, not
their helper functions, not their fixture constants. Keep the text, which is often the most valuable
comment in a file, and change only the marker.

**A `pub` item keeps `///` wherever it lives**, including a helper in `crates/cli/tests/common.rs`.
That is documented API for whoever writes the next test, and `clippy::missing_panics_doc` wants its
`# Panics`. Scope by what the item **is**, never by which directory holds it: converting everything
under `tests/` once stripped `# Panics` off exactly those helpers.

On a private production item, `///` still shows only in hover and in a doctest run, so keep it to
what fits there. A paragraph is a comment wearing documentation syntax — put it in `//` beside the
code.
