# xtask

Verification instruments for this workspace, run as `cargo xtask <command>` or through the `just`
recipes named below. Each works on committed trees only: it extracts every commit it examines with
`git archive` into a work directory with its own `CARGO_TARGET_DIR`, so uncommitted changes are
never measured. Cargo's build directory is pinned to the same place, so a `build.build-dir` in your
cargo config cannot share intermediate artifacts between builds.

The work directory defaults to a new directory under `TMPDIR` and is kept, so its logs can be read
afterwards. `--work-dir` overrides it with a new or empty directory. Either must be absolute and
outside every checkout of the repository, linked worktrees included.

Every build, test and toolchain probe runs in its own process group under a deadline. The group is
killed when the child exits, when the deadline passes, and when xtask receives Ctrl-C, SIGTERM or
SIGHUP. The `just` recipes build xtask itself into `target/xtask`, so they do not wait on the lock
of the workspace's `target/`.

## percommit

```bash
just percommit main               # every commit since the merge base with main, except HEAD
just percommit main --self-test   # prove the check can fail first
```

Runs `just clippy` on each commit below the tip, one at a time, and prints one verdict per commit
with the path to its log. The commits are those `git rev-list <merge base>..HEAD` lists, so a merge
commit in the range brings its second parent's commits with it. The tip is left to `just check`,
unless `--include-tip` is given or the working tree differs from HEAD, untracked files included.
Then `just check` is not measuring HEAD, so HEAD is checked too.

Every commit is checked with HEAD's `Justfile`, which replaces the commit's own in its archive. A
commit that weakens its `clippy` recipe is therefore still held to the one CI runs at the tip.

| Verdict     | Meaning                                                                                        |
| ----------- | ---------------------------------------------------------------------------------------------- |
| `ok`        | cargo started `selfie`, reported no compile error, and `just clippy` exited 0                  |
| `FAIL`      | a compile or clippy error, or a non-zero exit after `selfie` was built                         |
| `NEVER-RAN` | no `Compiling selfie v` or `Checking selfie v` line, or the deadline passed; nothing was shown |

It exits 0 when every commit scores `ok`, 1 when any does not, and 2 on an error or a failed
self-test. The toolchain is probed once per run, in the first commit's archive.

`--self-test` appends a type error, then a warn-level clippy lint, to `crates/selfie/src/lib.rs` at
the merge base, which is already on `main`. It stops unless both fail with an `error` diagnostic
quoting the injected line. The second control is an error only while `-D warnings` is in force.

## mutate

```bash
just mutate path/to/spec.toml               # every mutation in the spec
just mutate path/to/spec.toml --only m1     # one of them
just mutate-self-test                       # prove the runner scores each case correctly
```

A spec lists mutations. Each one names a file, an anchor copied from the committed file, its
replacement, the package and the one cargo target holding the tests, and the tests expected to fail,
written exactly as libtest prints them. The target is `["--lib"]`, or `--bin`, `--test`, `--example`
or `--bench` followed by one name. A relative spec path is read from the directory `just` was run
in.

```toml
rev = "HEAD"              # optional: the commit archived, and the source of every anchor
build_timeout_secs = 1800 # optional
test_timeout_secs = 600   # optional

[[mutation]]
id = "every-rule"         # letters, digits, `-`, `_` or `.`; names the mutation's directory
file = "crates/selfie/src/package/service/steps.rs"
anchor = '''
        Shown::Every => package.listing_refusal(),
'''
replacement = '''
        Shown::Every => None,
'''
package = "selfie"
target = ["--test", "package_service_tests"]   # required: one test target
tests = ["a_spec_selfie_cannot_read::listings_refuse_by_the_environments_they_show"]
expect = "caught"                              # or "survived", for a documented survivor
```

Before anything is built, a mutation is refused if its anchor does not match exactly once in the
file at `rev`, overlapping matches included. One unmutated baseline per package and target must then
pass every named test, and each mutation runs that same set of tests. Each mutation runs in its own
archive with its own target directory. It builds with `--no-run`, and is refused unless rustc's
dep-info shows the mutated file was compiled into that build, which a file in a dependency of the
tested crate is. It then runs `cargo test --no-fail-fast -- --exact <tests>` under the test
deadline. Each run leaves `mutation.diff`, `build.log` and `test.log` in `mutations/<id>/`.

| Verdict     | Meaning                                                                                      |
| ----------- | -------------------------------------------------------------------------------------------- |
| `CAUGHT`    | every named test failed                                                                      |
| `SURVIVED`  | every named test passed                                                                      |
| `MIXED`     | some failed and some passed; each is listed                                                  |
| `NEVER-RAN` | refused, did not compile, timed out, or a named test did not run; the reason says which case |

A mutation matches when it scores `CAUGHT` and expects `caught`, or `SURVIVED` and expects
`survived`. The run exits 0 when every mutation matches, 1 when any does not, and 2 on an error or a
failed self-test.

A mutation that crashes the test process, with a stack overflow or an abort, scores `NEVER-RAN`: the
crash takes libtest's report with it, so no named test can be credited. Narrow such a mutation until
the test can report.

A `SURVIVED` still needs a human judgment: check in `mutation.diff` that the mutated line is
reachable and that its value is observed, because a substitution the compiler can discard changes
nothing and survives. The dep-info check does not settle reachability either: it records source
files, not `#[cfg]` items. A mutation inside a library's `#[cfg(test)]` code, scored against an
integration-test target, passes the check because the file is compiled, but the mutated code is not
in that binary, so it survives.

`--self-test` mutates `crates/xtask/src/control.rs` at HEAD with eight controls: a value flip that
must be caught, a comment edit that must survive, and six that must never run. Those six cover an
anchor that matches nothing, a type error, a test process that aborts, a hang past the deadline, a
test the baseline does not list, and a file the build never compiles, which is this README. The hang
spawns a child, and the self-test also checks that the child died with its process group.
