# 0015. An install command's timeout counts silence, and every other command's counts run time

Date: 2026-11-05

## Status

Accepted 2026-11-05.

## Context

`command_timeout` exists to stop a command that is stuck. A command that is slow but working, such
as a package compiling from source, is not stuck, and stopping it costs the whole install. A limit
on total run time cannot tell the two apart. Any value short enough to catch a hang quickly is too
short for a long build, and a value long enough for every build waits that long on every hang.

Install commands stream their stdout and stderr through selfie as they run, so selfie sees each sign
of progress when it happens. Check, audit and finder commands and dotfile `command:` sources are
buffered. They are short, and the ones that wait on a person (a biometric or password prompt) print
nothing while they wait, so a silence limit would behave the same as a total limit there. For a
content provider, a total limit is also the only bound on a command that never stops printing,
because its whole output is buffered.

These options were considered:

- **Silence for every command.** One meaning for the key. It removes the only bound on a runaway
  content provider and gains nothing for prompt-waiting commands.
- **Silence plus a separate total cap on installs.** This catches a command that loops while
  printing, such as a retry loop. It adds a second key that nearly every user leaves unset, and
  Ctrl+C already stops such a command.
- **A per-package timeout.** This fixes one slow package by hand, so every slow package has to be
  found by failing first.

## Decision

An install command, including each of its steps, is stopped after `command_timeout` seconds with no
output on either stdout or stderr. Every chunk read from either pipe restarts the count, and a chunk
that is not relayed to the caller still counts. The count covers the whole run. A command that
closes its pipes and keeps running, or that leaves a grandchild holding a pipe open, is stopped once
it has been silent that long. Time spent waiting for the user to answer one of the command's
terminal questions does not count, and the count starts over once the answer is sent. An install
command has no limit on total run time.

Every other command (check, audit, finder, dotfile `command:` sources and template bindings) is
stopped after `command_timeout` seconds in total.

A timeout names the limit that stopped it. An install that was silent too long is reported as having
produced no output for N seconds, and any other command as having timed out after N seconds. Both
name `command_timeout`.

## Consequences

- A long build that keeps printing finishes under the default limit. One build step that stays
  silent longer than the limit still fails. Tools that print nothing while they compile, such as
  Homebrew without `--verbose`, can do that, and the remedy is a larger `command_timeout`.
- An install command that keeps printing without making progress is never stopped by selfie. The
  user has to stop it with Ctrl+C.
- The key means two things depending on the command, so its documentation has to state both.
- Output decides the limit, so output a command buffers internally and has not yet written does not
  count. A tool that flushes rarely can look silent while it works.
