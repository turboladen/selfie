# 0006. One verdict, one exit-code table and one output rule for every command

Date: 2026-09-28

## Status

Accepted

## Context

Every command has to answer three questions: what exit code a run ends with, which lines a run
prints at default verbosity and which only with `--verbose`, and which stream each line goes to.
When each command answers them itself, two commands facing the same situation (a configured command
that exits non-zero, a step that waits on the network) report it differently, and a script cannot
rely on any of it.

Options considered and rejected:

- **One non-zero code for everything that is not clean.** Simpler, but a script cannot tell "your
  dotfiles drifted" from "selfie could not read your packages" without parsing output. `diff` and
  `grep` separate the two for the same reason.
- **The CLI inferring which steps wait on something external**, from step numbers or message text.
  It is what made the answers drift apart: the knowledge lives in the library, which starts the
  command or opens the connection.
- **Telling a signal-killed command from one that exited non-zero.** A user's login shell reports a
  killed child as an ordinary exit status (128 + the signal), so the distinction cannot be made
  reliably, and a rule that works only under some shells is worse than none.

This record states the answers once. A command that disagrees with it has a defect; the record is
not amended to match the command.

## Decision

### 1. One verdict, decided by the library

The library scores every completed operation as one of three outcomes. The CLI and the MCP server
both ask for it and never re-derive it.

- **Clean**: the command did what it was asked and found nothing to report.
- **Found**: the command did what it was asked, and the answer is something the user asked it to
  look for, or it told the user that part of what it was asked to cover was not covered.
- **Failed**: the command could not do what it was asked, or refused to.

Failed outranks Found, and Found outranks Clean. Cancellation is not an outcome; it is how a run
ends.

### 2. Exit codes

| code | meaning                                                                                              |
| ---- | ---------------------------------------------------------------------------------------------------- |
| 0    | Clean                                                                                                |
| 1    | Failed, including a refusal, the user declining a confirmation, and a run that ends without a result |
| 2    | Usage: a bad command line, or a prompt with no terminal to ask on                                    |
| 3    | Found                                                                                                |
| 130  | Cancelled (128 + SIGINT)                                                                             |

A new code takes the next free value from 3 to 63. It never takes 64-78 (`sysexits.h`) or 126 and
above (the shell reserves them), and no code is ever renumbered or reused.

### 3. What each command reports

| command                                   | Clean (0)                                                               | Found (3)                                                                 | Failed (1)                                                               |
| ----------------------------------------- | ----------------------------------------------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `apply`                                   | deployed, up to date, or conflicts skipped                              | never                                                                     | a refused entry; an error; a write that could not be recorded            |
| `dotfiles drift`                          | in sync; nothing deploys on this machine                                | drift; an orphaned target; the orphan check could not finish and said why | a refused entry; an error                                                |
| `package install`                         | installed, including when a recommended package failed (warned)         | never                                                                     | an install command failed                                                |
| `package check`                           | the check command exited 0                                              | the check command exited non-zero, for any reason (not installed)         | no check command; selfie's own timeout; the command could not be started |
| `package audit`                           | no conflict                                                             | a conflict; not installed                                                 | the audit command exited non-zero; no audit command                      |
| `package audit --all`                     | every audited package clean; a package with no audit command is skipped | any Found                                                                 | an audit command exited non-zero; a spec left out                        |
| `spec validate`                           | no warnings                                                             | warnings                                                                  | errors; the spec does not parse                                          |
| `config validate`                         | no warnings (informational notes do not count)                          | warnings, including unrecognized keys                                     | errors; the file cannot be loaded                                        |
| `spec create`, `spec edit`, `spec remove` | done                                                                    | never                                                                     | the user declined; an error                                              |
| every listing and query command           | answered                                                                | never                                                                     | an error                                                                 |

A configured command's exit status is the whole of what selfie knows about it. A command killed by a
signal reaches selfie through the user's shell as an ordinary non-zero status, so selfie treats
"exited non-zero" as one fact and does not try to tell a kill from an exit. A note that needs no
action, such as a state directory selfie will create on its first write, is informational and never
makes a run Found.

### 4. Streams

- **stdout carries the answer**: result tables, result cards, and the one summary line.
- **stderr carries everything about the run**: status lines, warnings, errors, the remedy for an
  error, and debug logs.

A script that captures stdout gets the answer and nothing else, at any verbosity.

### 5. What prints at default verbosity

- **Every run prints its result.** A command that succeeds prints at least one line on stdout saying
  so, even when there is nothing else to show.
- **A step that waits on something outside selfie says so, at every verbosity.** That means a
  configured command, a provider command, or the network. On a terminal it is a spinner; without one
  it is one status line on stderr. The library marks such a step as waiting when it emits it. The
  CLI does not infer it from step numbers or message text.
- **Everything else about the run is hidden unless `--verbose`**: the operation header, local steps,
  and debug logs. `cli: verbose: true` in the config file is the same switch as `--verbose`.
- **The summary line names the environment** for every command whose answer depends on it.

### 6. Paths

- A source is shown relative to its base directory, the package directory or the dotfiles directory.
  The base directory is printed once, before the first line that uses it. The library reports every
  source already resolved against its base directory, so the CLI never guesses which base a relative
  spelling belongs to.
- A target is shown with the home directory as `~`, and only when the path is inside it.
- A path under no known base is shown in full.

## Consequences

- Scripts and shell prompts can rely on the exit code without reading output: 3 means "look", 1
  means "broken".
- A new command gets these rules by emitting the right events; the CLI does not decide per command
  what is quiet.
- The library's progress event gains a waiting marker, and the source paths it reports are resolved.
  Both are changes to the event types that every adapter follows.
- The MCP server reports the same outcome, and its tool descriptions must match section 3.
- Costs: a script that treated exit 0 from `dotfiles drift`, `package audit` or either validator as
  "nothing to see" now sees 3 for a finding. Status lines move from stdout to stderr, so a pipeline
  that read them from stdout no longer sees them. A check command that exits non-zero because it was
  killed reads as "not installed" rather than as a failure.
