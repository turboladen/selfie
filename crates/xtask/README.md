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
