# 0007. Write the deploy state before the first write, and install only what can be installed

Date: 2026-10-02

## Status

Accepted

## Context

Two commands can do work they cannot finish.

- **apply and track record what they write in the deploy state.** If the state cannot be written, a
  write made first leaves a deployed file unrecorded, which `dotfiles drift` then calls "not
  tracked", or leaves track's copy and spec in place with no record.
- **install works through an install order, dependencies first.** A package that cannot be installed
  in this environment, because its install command is blank (`install: ""`, or spec create's
  `# TODO`) or it has no entry for the environment, is found only when its turn comes, after the
  packages ahead of it have installed.

Options considered and rejected:

- **Checking permissions up front, and nothing more.** `access(W_OK)` passes a sticky directory, an
  immutable state file and a full disk, all of which still fail the save after a target is written.
- **Writing the state back lazily, before the first repo-file deploy or record.** Secret-bearing
  entries never record, so they could write a credential before the write-back failed, and the run
  would still say nothing was deployed.
- **Refusing a blank install outright.** An already-installed dependency whose spec still says
  `# TODO` would then block every package that depends on it, though nothing about it needs
  installing.

## Decision

### 1. The deploy state is written once, before the first write of any kind

A real `apply` writes the deploy state back just before its first write of any kind: a
secret-bearing target, including one rewritten only to tighten its mode, a backup, a repository-file
target, or a record. If that write fails, the run stops with nothing written. A run that writes
nothing never touches the state, so a read-only state directory does not stop it. The end-of-run
tidy, which drops records of files that are gone, is itself a write of the state; when it fails, the
run warns and keeps its result. `dotfiles track` writes the state back just before it copies the
file, after its own refusals.

A dry run writes nothing. It checks whether the state directory can be written, by its permissions,
and warns when a real run that writes anything would stop.

### 2. config validate warns about a state directory it cannot write

A state directory selfie cannot write into or create is a **warning** in `config validate` (exit 3),
not an error. A run that writes nothing is not stopped by it, so the configuration is usable; a run
that writes anything stops before it does.

### 3. The writability check is a port method built on directory_state

`FileSystem::file_creation_refusal` is a provided trait method: it walks from the directory to the
nearest ancestor that exists, through `directory_state`, refusing a file, a dangling link or an
unknown path in the way, and asks the one required primitive, `access_refusal`, about the directory
it reaches. Test decorators that change `directory_state` therefore change this answer too.

### 4. A package is installable here, or it is refused before anything installs

Dependency resolution, which already loads every package in the install order, records a verdict for
each one from its spec alone:

| verdict     | when                                                                      |
| ----------- | ------------------------------------------------------------------------- |
| install     | the current environment has a non-blank install command                   |
| check-first | the install command is blank, and there is a check command                |
| refused     | the current environment is missing, or the install is blank with no check |

Before any install command runs, `package install` refuses the first `refused` package, naming it,
and then runs the check of each `check-first` package. A check that says the package is installed
makes it installable: its turn reports it already installed, without running the check again. Any
other answer refuses it, naming it, before any install command runs. A recommend is resolved the
same way, and a refused recommend is reported as a failed recommend; its dependencies are not
installed.

These checks are the one thing that runs before the refusal, because the rule asks what the check
says.

## Consequences

- No deployed file goes unrecorded because of a state selfie could not write, unless a write fails
  part way through, on a full disk for example; that still stops the run after the one file.
- Every real apply that writes anything makes one extra durable write of the state.
- A packages-only setup over a read-only state directory validates with a warning, not an error.
- A dependency missing the current environment is refused before anything installs.
- A refusal names the package it is about, which may be a dependency rather than the package the
  user named.
