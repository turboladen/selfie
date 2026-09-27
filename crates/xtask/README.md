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

## bindiff

```bash
just bindiff main                     # the starter fixtures in crates/xtask/fixtures/bindiff
just bindiff main path/to/fixtures    # a fixture directory of your own
just bindiff-self-test                # prove the harness sees a difference and refuses an escape
```

Builds `selfie` at the merge base of the named commit with HEAD, and at HEAD, side by side, each
from its own archive and target directory. It refuses to run when the merge base is HEAD itself,
since there would be nothing to compare. Then it runs every fixture against both binaries in
identical, fresh sandboxes, and prints each fixture's verdict as it finishes, with a unified diff
when the transcripts differ. It ends with the list of fixtures covered and the commands each ran. A
diff covers the inputs it was given and nothing else, so that list is the claim a PR can make.

The starter fixtures are read as committed at HEAD. A fixture directory you name is read from disk.

| Verdict     | Meaning                                                                   |
| ----------- | ------------------------------------------------------------------------- |
| `identical` | both transcripts match                                                    |
| `DIFFERS`   | the transcripts differ; the diff follows                                  |
| `REFUSED`   | a fixture rule or the gate kept the fixture from running; the reason says |
| `NEVER-RAN` | the gate or a run hit its deadline, so nothing it did was compared        |
| `ERROR`     | the fixture could not be laid down or the binary could not be run         |

It exits 0 when every fixture is identical, 2 when any scored `ERROR` or the run itself failed, and
1 otherwise.

A fixture is one TOML file. `@HOME@` stands for the sandbox's path in file content, run arguments
and link targets. Without a file of its own under `.config/selfie/`, a fixture gets the same default
config as `just sandbox-run`, with an empty `packages/` directory.

```toml
observe = ["target/credentials"]   # reported after every run, never followed through a symlink
timeout_secs = 60                  # optional; for the gate and for each run

[[dir]]
path = "locked"
mode = 0o300                       # optional; directory modes are applied last, deepest first

[[file]]
path = "packages/creds.yaml"
content = "..."
mode = 0o600                       # optional

[[symlink]]
path = "target/gone"
to = "@HOME@/target/nowhere"       # must resolve inside the sandbox

[[run]]
args = ["--no-color", "apply", "--yes"]

[[run]]
args = ["--no-color", "dotfiles", "drift"]
```

The runs execute in order in one sandbox, so state carries from one to the next. Each binary runs
with the same environment `just sandbox-run` sets, with `TERM=dumb`, a `TMPDIR` of its own and git
discovery stopped at its sandbox, from the sandbox directory, with stdin closed. stdout and stderr
are captured separately. The sandbox path is replaced by `<HOME>` and RFC 3339 timestamps by
`<TIME>`; any other content that varies from run to run, such as a timestamp in another format,
shows as a difference. An observed path is read only if it is a regular file.

### Containment

Every invocation of the binary under test, the gate included, runs under macOS `sandbox-exec`. Its
profile starts from allowing everything and denies:

- writes anywhere but that side's sandbox and the `/dev` nodes a process writes its own output to;
- reads under your home directory, other than the work directory;
- opening a terminal, so a command cannot queue input for your shell;
- the network, unix sockets included;
- Mach services, LaunchServices and Apple events, any of which would run code outside the sandbox on
  the command's behalf;
- signals to processes outside the sandbox, and reading their command lines.

Child processes inherit it, so `install`, `check` and `audit` commands and `command:` dotfile
sources are confined too, and so is any process that outlives its run. Such a process can still
change its own sandbox between runs, which shows as a difference. Output is captured through pipes
and kept up to 4 MiB a stream, and an observed file up to 1 MiB, so nothing the binary writes can
fill the disk or memory outside its sandbox. A command that needs anything the profile denies fails
rather than reaching it. bindiff refuses to run on any other OS, and refuses when it cannot find
your home directory.

The profile names real paths, because the kernel matches the path it resolved: a rule naming the
`/tmp` alias of `/private/tmp` would match nothing. Every path in it is canonicalized first, and one
that cannot be is refused.

Only the binary is confined. The harness's own writes, which lay the fixture down and observe it
afterwards, run unconfined, so they keep their own guards: fixture paths are relative, without `.`
or `..`; directories and files are written before any symlink; and no write passes through a
symlink. An observation walks directory descriptors opened without following links, so a process the
binary left running cannot swap a checked path for a link before the harness reads it.

A fixture is also refused before anything runs when:

- a run passes `--package-directory`, `--dotfiles-directory`, `--state-directory` or `-p`, or names
  an absolute path outside `@HOME@`;
- a config file, or a spec's `target:` or `source:`, names such a path, a `~user` path, or a path
  with `..`;
- a symlink it made resolves outside the sandbox, following chains.

These rules read text, so a spelling they do not know, such as a YAML anchor, passes them. They save
a run; the OS is what keeps the binary in.

Both sides are laid out and gated before either runs. The gate runs `config validate`, which
executes nothing, and requires it to succeed. Each directory it prints must appear once, and resolve
inside the sandbox one component at a time in the order the kernel follows links. A binary that does
not print a directory, as older ones do not, passes only when the config leaves that directory at a
default inside the sandbox. A fixture whose config does not validate is refused, so config errors
cannot be diffed.

`--self-test` builds HEAD once and runs these controls:

- The starter fixtures must come out identical against HEAD itself, deploy state and its timestamps
  included.
- A package planted on one side only must show as a difference naming it, which proves the differ
  can see one; it is not a binary diff.
- Two stub binaries that print different lines must show as a difference, which a harness running
  one binary on both sides would not.
- Fixtures that would leave the sandbox must stay in it. Some must be refused early, some by the
  gate with the early refusals off. Four must run and be stopped by the OS: a dotfile target behind
  a YAML anchor and a `--state-directory` flag, both aimed at a directory the binary may read but
  not write, and `dotfiles track` of a file it may not read and of one in a stand-in home directory.
  Each stopped run must fail with "Operation not permitted" on the line naming its escaping step.
  Nothing a sandbox holds or a run printed may contain the targets' marker, and every target
  directory must be unchanged afterwards, entry by entry.

The write, home and unreadable-directory rules each have a self-test control that fails without
them. The terminal, network, Mach and signal rules each have a unit test, run on macOS, that fails
without them; the process-information rule is a second barrier behind the Mach rule and has none of
its own.

`selfie-mcp` is not covered: only the `selfie` binary is built and run.
