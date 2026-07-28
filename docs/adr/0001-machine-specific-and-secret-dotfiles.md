# 1. Per-environment and externally-sourced dotfiles

Date: 2026-07-23

## Status

Accepted

Refined by [ADR-0003](0003-no-deploy-state-for-provider-sourced-dotfiles.md), which specifies the
deploy-state handling deferred below, and [ADR-0004](0004-named-value-substitution-for-dotfiles.md),
which revisits the rejection of templating for the case of a secret held within a structured file.

## Context

A package's `dotfiles` are deployed to their targets by `selfie apply`. Today `dotfiles` is a single
top-level list that applies identically in every environment, even though the rest of the package
model — `install`, `check`, `audit` — is already scoped per environment under `environments.<name>`.
Dotfiles are the inconsistent exception.

Two real needs cannot currently be expressed:

- **Machine-specific configuration.** A target may need to exist only in some environments (for
  example a corporate proxy config present only on a work machine), or hold different content in
  different environments (for example a tool config that legitimately differs between machines).
- **Secret-bearing configuration.** Some config content is credentials that must not be stored in a
  package repository — which may be public — yet must still be deployed on the machine that needs
  it.

selfie's design principle is to orchestrate user-defined commands rather than reimplement what
established tools already do (it runs the user's own install/check commands instead of being a
package manager). The same principle applies here: selfie should not grow its own templating
language or secret store when mature external tools (password managers, secret CLIs) already solve
secret retrieval.

Per-machine _git identity_ is intentionally out of scope: git's native `includeIf "gitdir:…"`
already selects identity per repository location within a single shared config, on any machine.

## Decision

### Per-environment dotfiles

`EnvironmentConfig` gains an optional `dotfiles` list, mirroring the shape of the top-level one. The
set of dotfiles applied in the active environment is the top-level (shared) list combined with that
environment's list, resolved by `target`:

- an environment entry whose `target` matches a shared entry **replaces** it (variant);
- an environment entry with a new `target` is **added** (presence);
- shared entries with no environment override apply unchanged.

Source paths continue to resolve relative to the package file's directory. There is no mechanism to
_exclude_ a shared entry from a single environment: a config that is not universal belongs in the
relevant environment lists rather than the shared list.

### Externally-sourced dotfile content

A dotfile's content may come from either a file in the package repository or from the standard
output of a user-configured provider command run at deploy time (for example a password-manager or
secrets-CLI invocation). selfie writes that output to the target and never records the resolved
value in the repository or in deploy state. selfie does not implement encryption, secret storage, or
templating itself.

To support both, `DotfileEntry` models its content source as an abstraction over a repository file
and a provider command, rather than assuming a repository file.

### Overwrite safety

`selfie apply` must never discard configuration a user may have authored. When a target already
exists, selfie compares it against both the content it would write and its own record of what it
last deployed:

- target absent, or identical to the source content → write or skip; nothing is at risk.
- target unchanged since selfie last deployed it → update it to the current source; the user has not
  modified it, so this is a normal refresh.
- target differs and selfie has no record of having deployed it (a first deployment, or a new
  machine), or the target was modified since selfie last deployed it → this is a **conflict**.
  selfie shows a diff and does not overwrite. Where an interactive prompt is available the user
  decides; where one is not (a non-interactive caller), selfie skips the file and reports the
  conflict rather than overwriting it.

The non-negotiable guarantee is that divergent, possibly user-authored content is never replaced
without explicit consent. Offering richer resolution than accept-or-skip — for example backing up
the existing file before overwriting, or a merge step — is a possible future enhancement; the diff
is already produced, while merging is the harder part.

## Consequences

- Dotfiles become consistent with the per-environment package model; a single package can describe
  shared configuration plus environment-specific presence and variants.
- Machine-specific and secret-backed configuration become expressible without a templating language
  or a built-in secret store.
- The deploy path must compute the per-environment effective set and, for provider-sourced entries,
  execute the provider command. Deploy-state and drift detection must not persist a secret's value
  or checksum; that path needs its own handling, specified when it is built.
- Validation must surface environment overrides and missing sources rather than applying them
  silently.

## Alternatives considered

- **A templating engine** (variable substitution in config files): rejected. It duplicates what
  external tools already provide and is heavier than the observed need, where nearly all
  configuration is genuinely shared once paths are written relative to the home directory.
- **Alt-file naming conventions** (encoding the environment in the filename, e.g. `config##work`):
  rejected as less explicit and discoverable than environment blocks, and inconsistent with the
  existing per-environment model.
- **A built-in secret store or encryption**: rejected as a reimplementation of established secret
  managers, which the provider-command integration point reuses instead.
