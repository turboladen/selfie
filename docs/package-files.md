# Package Files Reference

This document provides a comprehensive reference for creating and managing selfie package definition
files.

## Overview

Package files are YAML documents that define how to install and check packages across different
environments. They serve as the core configuration that tells selfie how you prefer to manage each
tool or library.

## File Structure

Package files follow this basic structure:

```yaml
name: package-name
description: (optional) Brief description of what this package provides
homepage: (optional) https://example.com
post_install_note: (optional) Note displayed after first-time install

dotfiles:
  - source: package-name/config.toml
    target: ~/.config/package-name/config.toml

environments:
  environment-name:
    install: command to install the package
    check: (optional) command to verify installation
    dependencies:
      - dependency-package-1
      - dependency-package-2
    recommends:
      - optional-companion-tool

  another-environment:
    install: different installation command
    check: verification command
```

## Required Fields

### `name`

The package name. It should agree with the file name without its extension. Both `.yml` and `.yaml`
are accepted.

selfie resolves a package from the **file name**, not from this field, so `Neovim.yml` answers to
`neovim` whatever `name:` says. `selfie spec validate` reports a disagreement between the two as a
warning and still loads the file.

```yaml
name: ripgrep # for file ripgrep.yaml
```

Names are compared ignoring case, so `neovim` and `Neovim` are one package: a spec stored as
`Neovim.yml` answers to either. The extension folds the same way, and does not distinguish one
package from another — `Neovim.YML`, `neovim.yml` and `neovim.yaml` all name the package `neovim`.

A directory holding two of them is reported as ambiguous rather than resolved silently, and
`selfie sync push` refuses to carry it.

Two capitalizations of one name are worse than two extensions, and the refusals say so. `neovim.yml`
and `neovim.yaml` both survive any checkout, and the package is merely unresolvable until one of
them is renamed. `Neovim.yml` and `neovim.yml` cannot both survive a checkout on a case-insensitive
file system: cloning that directory there keeps one file and discards the other with no diagnostic.
That makes the capitalization half a portability requirement rather than a style preference.

### `environments`

A map of environment names to their installation configurations. At least one environment must be
defined.

```yaml
environments:
  macos:
    install: brew install ripgrep
    check: which rg
```

## Optional Fields

### `description`

Brief description of what the package provides.

```yaml
description: Fast text search tool that respects gitignore
```

### `homepage`

URL to the package's homepage or repository.

```yaml
homepage: https://github.com/BurntSushi/ripgrep
```

### `dotfiles`

A list of dotfile mappings that define files to deploy from your dotfiles repository to their target
locations. Unlike environment-specific fields, `dotfiles` is a top-level field that applies across
all environments.

Every entry has a `target` — the destination path, either `~/…` or absolute — plus one of three ways
of saying where the content comes from.

Prefer `~/` for anything under your home directory. `~/.gemrc` names the same file on every machine;
`/Users/you/.gemrc` names it on one. An absolute path is for destinations that are genuinely the
same everywhere, such as `/etc/nginx.conf`. selfie deploys as whoever runs it and never elevates, so
a target like that one needs write access you may not have.

**`sudo selfie apply` is not the answer, and selfie refuses it.** There is no per-entry privilege
scope, so the whole run would be written by whoever sudo switched to — including every `~/` entry,
leaving files you no longer own in your own home directory. Worse, `~` may not even mean your home
directory: expansion reads `$HOME`, and on a machine whose sudoers policy resets it, the dotfiles
land in `/root` and the run reports success. A target you cannot write stays one failed entry, which
is the smaller problem. The same refusal covers `selfie track`, `selfie dotfiles track` and
`selfie package track-dotfile`. `--allow-sudo` overrides it, for the case where you do mean every
target to be written by that user. `sudo -u alice` is refused on the same grounds, and for the same
reason.

The `~user/…` form — another user's home directory — is **not** supported. selfie expands `~/` for
whoever is running it and nothing else. `selfie spec validate` reports a `~alice/.gemrc` target as
an error, and `selfie apply` refuses it rather than deploying it somewhere.

```yaml
dotfiles:
  # 1. A repository file, copied as-is.
  - source: fnm/fish-conf.fish
    target: ~/.config/fish/conf.d/fnm.fish

  # 2. A repository file rendered as a template, with named values from commands.
  - source: rubygems/credentials.tpl
    target: ~/.gem/credentials
    vars:
      api_key: op read op://Private/rubygems/token
      corp_token: teller get GEMSERVER_TOKEN

  # 3. A command whose entire standard output is the file.
  - command: op read op://Private/ssh-key/private
    target: ~/.ssh/id_ed25519
```

- `source` — Relative path from the YAML file's parent directory (source files are colocated
  alongside their package or dotfile definition)
- `command` — A command whose stdout becomes the whole file
- `vars` — A map of names to commands, rendering `source` as a template

Set **exactly one** of `source` or `command`. `vars` goes only with `source`: with `command` there
is no template to render, and that combination is rejected rather than ignored.

Any other key inside a dotfile entry — except an underscore-prefixed [YAML anchor](#yaml-anchors)
that is not named after one of these fields — is an error, and the entry is skipped rather than
deployed. Only `target` is required, so a misspelling would otherwise be dropped silently: writing
`var:` for `vars:` would leave a valid-looking repository-file entry and deploy the template
_unrendered_, placeholders and all, over the file it was meant to fill in. See
[Unrecognized keys](#unrecognized-keys).

Dotfiles are deployed with `selfie apply`, not during `selfie package install`. This separation
keeps installation fast and gives you explicit control over when dotfiles are written to disk.

See [Dotfile Deployment](#dotfile-deployment) below for the full workflow, and
[Provider-sourced and templated dotfiles](#provider-sourced-and-templated-dotfiles) for entries
whose content comes from a command.

#### Environment-specific dotfiles

The top-level `dotfiles` list is **shared** — it applies in every environment. When a config must
differ per machine, add a `dotfiles` list inside an `environments.<name>` block. For the active
environment, its entries are combined with the shared ones by `target`:

- an entry whose `target` matches a shared entry **overrides** it (a variant), and
- an entry with a new `target` is **added** for that environment only (present only there).

```yaml
dotfiles:
  - source: bat/config # shared: deployed in every environment
    target: ~/.config/bat/config

environments:
  macos-home:
    install: brew install bat
  macos-work:
    install: brew install bat
    dotfiles:
      - source: bat/work.config # overrides the shared bat config on work only
        target: ~/.config/bat/config
      - source: zscaler/work.conf # present only on work
        target: ~/.config/zscaler/config
```

There is intentionally no way to _exclude_ a shared entry from a single environment: a config that
is not universal belongs in the relevant `environments.<name>.dotfiles` lists rather than the shared
list. Per-machine differences that a single portable file can express — `~`-relative paths, runtime
evaluation like `$(brew --prefix)`, or git's native `includeIf` for identity — are preferable to a
variant.

### `post_install_note`

An optional message displayed to the user after a fresh install. Use this for important setup steps
that can't be automated, like adding shell integrations or restarting services.

```yaml
post_install_note: |
  Add this to your shell profile to activate fnm:
    eval "$(fnm env --use-on-cd)"
```

The note is only shown on first-time installs, not on subsequent runs where the check command
already passes.

## Environment Configuration

Each environment must have an `install` command. Everything else is optional: a `check` command, an
`audit` command, `dependencies`, `recommends`, and `dotfiles`.

An environment accepts only `install`, `check`, `audit`, `dependencies`, `recommends` and
`dotfiles`. `selfie spec validate` reports any other key and names the environment it is in, so a
misspelled optional key such as `audt:` is caught rather than ignored.

`selfie apply` refuses a package whose _applied_ environment carries such a key, rather than
deploying from it. An `_dotfiles:` there would otherwise leave that environment's list empty, so the
shared entry would deploy over the file the environment meant to override. A key in an environment
the run does not apply is left alone. The commands that rewrite a package file —
`selfie package track-dotfile` and the MCP `spec_update` tool — refuse for the same reason: the key
is not modeled, so rewriting from the struct would delete it silently. The same refusal covers a key
at the file's top level, where a rewrite would take every entry under it.

`selfie spec edit` refuses the same way. A file that will not parse is not an absent package, and
treating it as one offered to create a template over the file the user opened the editor to repair.

Creating a package refuses too when a file is already at that path and selfie cannot read it. Only a
name with no file behind it is a create; anything else — a file that will not parse, one selfie
refused to open, two files claiming the same name — would be overwritten, and the guards above
cannot see it because the package being written was built in memory rather than read from disk.

`selfie spec create` and `selfie spec edit` also refuse when the path is already taken, which is not
the same question as whether the name is. Package names match exactly, so an existing `Neovim.yml`
does not answer to `neovim` — but on a case-insensitive file system, writing `neovim.yml` resolves
to that same file. selfie asks the file system rather than comparing names, so on a case-sensitive
file system the two remain separate packages and both can exist.

The rewriting commands also refuse a file selfie could not read back, even though no key is known to
be wrong with it. Apply is the other way round: it warns and deploys such a file rather than stop
over a check that did not run, and skips it only when there is nothing left to deploy. A declined
write costs a retry, while rewriting would delete whatever the file carries that selfie does not
model.

`selfie spec edit` is not among them, because it does not rewrite. It opens an existing file exactly
as written, so anchors, comments and key order survive editing, and a file carrying a key selfie
would refuse to write can still be opened to fix it. Only a package that does not exist yet is
written before the editor opens.

Keys beginning with `_` are treated as YAML anchor definitions and allowed, unless the rest of the
name matches a real field — `_check:` cannot be told apart from a misspelling of `check:` and is
refused.

### `install`

Command(s) to install the package. Can be a single command or multi-line script.

**Single command:**

```yaml
install: brew install ripgrep
```

**Multi-line script:**

```yaml
install: |
  curl -Lo ripgrep.tar.gz https://github.com/BurntSushi/ripgrep/releases/download/13.0.0/ripgrep-13.0.0-x86_64-unknown-linux-musl.tar.gz \
    && tar xzf ripgrep.tar.gz \
    && sudo cp ripgrep-13.0.0-x86_64-unknown-linux-musl/rg /usr/local/bin/
```

> **Working Directory**: All install and check commands automatically run in the package directory
> (where the `.yaml` file is located). This means you can use relative paths like
> `./scripts/install.sh` without needing to manually change directories.

**Multi-step with error handling (recommended):**

```yaml
install: |
  set -e
  echo "Downloading Node.js..."
  curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
  echo "Installing Node.js..."
  sudo apt-get install -y nodejs
  echo "Verifying installation..."
  node --version
```

> **Important:** For multi-line scripts, consider adding `set -e` at the beginning to ensure the
> script exits immediately if any command fails. This prevents subsequent commands from running
> after a failure, which is critical for package installation integrity. Selfie will warn you if it
> detects multi-line commands without error handling to help you write safer installation scripts.

### `check`

Optional command to verify the package is installed correctly. Should exit with code 0 if installed,
non-zero if not. If not given, executing the `check` command will always behave as if the package is
not yet installed.

**Simple binary check:**

```yaml
check: which rg
```

**Version verification:**

```yaml
check: node --version | grep -q "v20"
```

**Complex verification:**

```yaml
check: |
  which docker && \
    docker --version && \
    docker info > /dev/null 2>&1
```

**Service status check:**

```yaml
check: systemctl is-active --quiet docker
```

### `dependencies`

List of other selfie packages that must be installed before this package.

```yaml
dependencies:
  - node
  - python
  - homebrew
```

Dependencies are installed in the order listed, and selfie will recursively install their
dependencies first.

### `recommends`

Optional list of packages to install as soft dependencies. Unlike `dependencies`, a recommend
failure does **not** prevent the parent package from succeeding.

```yaml
recommends:
  - node # Useful companion, but fnm works without it
  - yarn # Nice to have alongside
```

**Key differences from `dependencies`:**

| Behavior         | `dependencies` | `recommends`      |
| ---------------- | -------------- | ----------------- |
| Install order    | Before parent  | After parent      |
| Failure handling | Parent fails   | Warning only      |
| Depth            | Recursive      | One level only    |
| Skip flag        | —              | `--no-recommends` |

Recommends are installed by default. To skip them:

```bash
selfie package install fnm --no-recommends
```

Recommends are only one level deep — a recommended package's own recommends are not followed. This
keeps installation predictable and avoids deep recursive recommend chains.

## Command Execution

### Working Directory

All install and check commands automatically run in the **package directory** (where the `.yaml`
file is located). This means you can use relative paths to reference scripts, configuration files,
or other resources without needing to manually change directories.

**Example with relative paths:**

```yaml
environments:
  macos:
    install: |
      set -e

      # These paths are relative to the package directory
      source ./scripts/common.sh
      ./scripts/macos_install.sh
    check: ./scripts/check.sh

  ubuntu:
    install: ./scripts/ubuntu_install.sh
    check: ./scripts/check.sh
```

**Directory structure:**

```
package-directory/
├── my-package.yaml        # Package definition
├── other-package.yaml     # Other packages
└── scripts/               # Scripts directory
    ├── common.sh          # Shared utilities
    ├── macos_install.sh   # macOS installation
    ├── ubuntu_install.sh  # Ubuntu installation
    └── check.sh           # Verification script
```

### Error Handling

For multi-line scripts, consider adding proper error handling to ensure scripts exit immediately if
any command fails:

```yaml
install: |
  set -e  # Exit on first error
  echo "Installing package..."
  command1
  command2
  echo "Installation complete"
```

### Command Output

**Your commands' output is shown to whoever is running selfie. Write them as if everything they
print will be read.**

- **`install` output is streamed live**, line by line, as the command runs — both stdout and stderr,
  whether the command succeeds or fails. When selfie is driven through its MCP server rather than
  the terminal, those same lines are included in the structured response the AI assistant receives.
- **`check` output is displayed** when you run `selfie package check`, so that a check reporting
  "not installed" can tell you why.

This is deliberate: watching an install run and reading a check's explanation are the point. But it
means **a command that prints a credential prints it to the terminal and to any connected MCP
client**. If a command needs a token, pass it through the environment or have the command read it
directly — do not `echo` it.

Failure reports are narrower. When a command exits non-zero, what selfie reports about the failure
itself is the command and its exit code — and its standard error, cut to 2000 bytes of what the
command wrote. It does not repeat the command's standard output. The same limit applies to a failing
`command` or `vars` dotfile entry and to the message a failing `audit` command produces.

The cut keeps **both ends** — the first 1000 bytes and the last 1000 — and replaces the middle with
a marker naming how much went:

```text
… (3200 bytes elided) …
```

Both ends, because a failing command usually explains itself on its last line. A `brew install` that
prints pages of `==> Downloading` progress and then one `Error:` line keeps that `Error:` line;
cutting only the first 2000 bytes would have kept the progress and dropped the reason. The head is
worth keeping too, since it is where a command says what it was attempting.

The two cuts land on byte boundaries rather than character boundaries, so a multi-byte character
straddling either one is replaced rather than split — the head can end mid-character and the tail
can begin mid-character. Output only slightly over the limit is passed through whole, since eliding
it would cost more in marker text than it saved.

The 2000 counts what the command wrote, not the length of the message you read: invalid UTF-8 is
replaced character by character, so binary output can render longer than the bytes it came from.

Where that standard error travels depends on which report carries it. An install or check failure
shows it in the terminal only — the value an MCP client receives names the command and its exit code
and stops there. An `audit` error is different: its message reaches a connected MCP client as well,
which is why the same limit applies to it. Anything else you see about a failed command reached you
through one of the channels above, as it ran.

If a dotfile's content is itself a secret, use a `command` or `vars` dotfile entry rather than an
install command — selfie treats those as secret-bearing and never puts their content in a message, a
log line, or an error. See
[Provider-sourced and templated dotfiles](#provider-sourced-and-templated-dotfiles).

## Advanced Features

### Environment Variables

You can use environment variables in your commands:

```yaml
install: |
  set -e

  export VERSION=${TERRAFORM_VERSION:-1.6.0}
  wget https://releases.hashicorp.com/terraform/${VERSION}/terraform_${VERSION}_linux_amd64.zip
  unzip terraform_${VERSION}_linux_amd64.zip
  sudo mv terraform /usr/local/bin/
```

### Conditional Logic

Use shell conditionals for platform-specific behavior within an environment:

```yaml
install: |
  if [[ "$OSTYPE" == "darwin"* ]]; then
    brew install postgresql
  else
    sudo apt-get install postgresql-client
  fi
```

### User-specific Installation

Install tools in user space rather than system-wide:

```yaml
install: |
  mkdir -p ~/.local/bin \
    && curl -Lo ~/.local/bin/jq https://github.com/stedolan/jq/releases/download/jq-1.6/jq-linux64 \
    && chmod +x ~/.local/bin/jq
check: ~/.local/bin/jq --version
```

### Post-installation Setup

Perform additional configuration after installation:

```yaml
install: |
  set -e
  brew install docker

  # Start Docker Desktop
  open /Applications/Docker.app

  # Wait for Docker to start
  while ! docker info > /dev/null 2>&1; do
    echo "Waiting for Docker to start..."
    sleep 5
  done

  echo "Docker is ready!"
```

## Dotfile Deployment

Packages can declare dotfiles that should be deployed to specific locations on your system. This is
useful for shell integrations, editor configs, tool settings, and anything that lives outside the
package directory.

### How It Works

1. **Define** dotfile mappings in your package YAML (the `dotfiles` field)
2. **Store** source files alongside the YAML definition (in a subdirectory named after the package)
3. **Deploy** with `selfie apply`

### Directory Structure

Source files are colocated with their YAML definitions. Package dotfiles live under `packages/`, and
standalone dotfiles (not tied to any package) live under `dotfiles/`:

```
~/.selfie/
├── packages/              # Package definitions (package_directory)
│   ├── fnm.yaml
│   ├── fnm/               # Source files for fnm package
│   │   ├── fish-conf.fish
│   │   └── zsh-conf.zsh
│   ├── starship.yaml
│   └── starship/          # Source files for starship package
│       └── starship.toml
└── dotfiles/              # Standalone dotfile definitions (dotfiles_directory)
    ├── dprint.yaml
    └── dprint/            # Source files for standalone dprint dotfile
        └── dprint.jsonc
```

### Example Package with Dotfiles

```yaml
name: starship
description: Cross-shell prompt
homepage: https://starship.rs

post_install_note: |
  Restart your shell or source your profile to activate starship.

dotfiles:
  - source: starship/starship.toml
    target: ~/.config/starship.toml

environments:
  macos:
    install: brew install starship
    check: which starship
    recommends:
      - nerd-fonts
```

### Deploying Dotfiles

```bash
# Deploy all dotfiles from all packages
selfie apply

# Deploy dotfiles for a specific package only
selfie apply starship

# Preview what would change without writing files
selfie apply --dry-run

# Overwrite even if target was modified locally
selfie apply --yes
```

### Conflict Detection

Selfie tracks checksums of deployed files. If you modify a deployed file locally _and_ the source
file changes, selfie detects this as a conflict:

```
⚠ Conflict: ~/.config/starship.toml
  Source and target both changed since last deploy.
  Use --yes to overwrite, or resolve manually.
```

Without `--yes`, conflicts are reported but the target file is left untouched.

### Symlinked targets

This section covers repository-file entries — a `source` with no `vars`. Provider-sourced and
templated entries never write through a link either, but their link is
[replaced rather than refused](#deploy-behavior-and-permissions).

selfie deploys by copying, so a symlink at a target is not a supported setup. When one is there and
selfie would otherwise write, it **refuses and skips that entry** with a warning naming the target
and where the link points. The link and the file it points at are both left exactly as they were,
and a dangling link's destination is not created. `selfie apply --dry-run` reports the same refusal
rather than previewing a deploy that would not happen.

```
⚠ Skipping 'git/gitconfig': /home/you/.gitconfig: target is a symlink to '/home/you/Sync/gitconfig' and selfie will not write through it
```

Writing through the link would send the content somewhere other than the path you configured —
possibly somewhere chosen by whoever created the link. Replacing the link instead would discard it
on the first apply after any edit to the repository file, which is not selfie's to decide.

The refusal applies only when selfie was going to write. A symlinked target whose contents already
match is in sync, so it is skipped as usual with no refusal reported. `--yes` does not lift the
refusal, and neither does answering an interactive conflict prompt — in fact you will not be asked,
since a refused entry is settled before the prompt. Overwriting a conflict and writing through a
link are separate questions, and `--yes` speaks only to the first.

**No deployment is recorded for a symlinked target**, including one that already matches. selfie did
not write it and never will, so an entry claiming otherwise would be a promise the refusal
guarantees it cannot keep. The visible consequence is in `selfie dotfiles drift`: the entry stays
[`not tracked`](#selfie-dotfiles-drift-reports-the-symlink-refusal) on every run rather than
settling to in sync, and `selfie sync status` keeps counting it. That is the honest report of a
config file selfie does not manage — and it is what replacing the link, or retargeting the entry at
the path the link points to, clears.

To put a target under selfie's management, replace the symlink with a regular file
(`rm ~/.gitconfig` before the next `selfie apply`, which then writes it), or point the entry's
`target` at the path the link points to.

A symlinked **parent directory** is still followed. Only the final component is checked.

### Fifo, socket and device targets

A target that is a named pipe (fifo), a socket, or a device node is refused rather than written to,
by `selfie apply`, `selfie dotfiles drift` and `selfie dotfiles track` alike:

```
⚠ Skipping 'myapp/config.toml': /home/you/.config/myapp/config.toml: target resolves to a named pipe (fifo) and selfie will not write to it
```

A symlink pointing at one of these is refused the same way — the message says _resolves to_ for that
reason. A **directory** at the target is not in this group; it is reported as an ordinary error.

#### `selfie dotfiles track` refuses a symlinked target

Tracking a symlinked target **fails** rather than recording it:

```
✗ /home/you/.gitconfig: target is a symlink to '/home/you/Sync/gitconfig' and selfie will not write through it. Replace the symlink with a regular file, or track the path it points to.
```

The refusal is deliberate, and it is stricter than a warning would be, because there is no
configuration in which tracking a symlinked target does what you asked. `selfie apply` will refuse
to write through it, so the entry would be either permanently inert or permanently broken.

Tracking reads the target to copy it into your dotfiles repository, and reading a symlink follows
it. Accepting one would therefore copy the contents of **the file the link points at** — which you
did not name, and which `selfie sync push` would then commit to your remote — and record a
deployment that never happened. Refusing before any of that is the point.

A dangling link is refused the same way, and is reported as a symlink rather than as a missing file.

#### `selfie dotfiles drift` reports the symlink refusal

`drift` names the symlink whenever `apply` would refuse the entry, using the same wording:

```
⚠   Drift in ~/.gitconfig: repo changed
⚠ Skipping 'myapp/config.toml': /home/you/.gitconfig: target is a symlink to '/home/you/Sync/gitconfig' and selfie will not write through it
```

The drift type on its own is misleading for a symlinked target, which is why the reason accompanies
it. Drift reads _through_ the link, so the checksum it compares belongs to the link's destination
rather than to the target — and because a refused entry never updates its recorded state, the same
drift is reported on every subsequent run. That permanence is real, not a display artifact: nothing
will clear it until the link is dealt with.

The **refusal** follows `apply`'s own decision, so it appears only where `apply` would refuse to
write. The **drift line** does not: an untracked target whose contents already match the repository
file is still listed as drifted, even though `apply` writes nothing and refuses nothing for it. That
line carries the reason as well, without the refusal:

```
⚠   Drift in ~/.gitconfig: not tracked — the target is a symlink, so selfie will not manage it and records no deployment for it
```

`selfie apply` says the same thing on its skip line for the same entry, so the two commands agree
about a target neither of them will ever manage.

That combination — `not tracked`, on every run, with the symlink reason beside it — is what a
symlinked target already in sync looks like, and running `selfie apply` does not clear it: nothing
is recorded for a target selfie will not write to, so there is no state for the entry to advance to.
[Symlinked targets](#symlinked-targets) explains what clears it.

Note that `selfie sync status` summarizes drift counts and does not carry this reason; it points at
`selfie dotfiles drift`, which does.

### Files in your repository

The same two hazards exist on the other side of the copy, in the repository selfie reads from.

A source that is a named pipe (fifo), a socket, or a device node is refused wherever selfie would
read it — `selfie apply`, `selfie dotfiles drift`, `selfie spec validate`, and the template behind a
[`vars:` entry](#provider-sourced-and-templated-dotfiles):

```
⚠ Skipping 'myapp/config.toml': the repository file is a named pipe (fifo) and selfie will not read it. Replace it with a regular file.
```

Git cannot store these, so a clone will not produce one — a local `mkfifo` or a restored backup can.

A symlink at a path selfie is about to write **into** your repository is also refused, which covers
`selfie dotfiles track` and `selfie spec create`/`update`/`edit`:

```
✗ Cannot write the tracked copy at '/home/you/dotfiles/gitconfig/gitconfig': it is a symlink to '/tmp/elsewhere'. Remove it, or track under a different name.
```

This holds wherever selfie writes a file itself. Git sync and the post-save formatter run through
separate tools and are not covered.

### Package files that are not regular files

The hazards above are about your dotfiles repository. Selfie refuses a package file that is a named
pipe (fifo), a socket, or a device node in the same way:

```
⚠ Invalid: /home/you/.selfie/packages/ghost.yml — the package file is a named pipe (fifo), not a regular file. Replace it with a regular file or remove it from the package directory.
```

Commands that enumerate specs continue with the rest and name the file they skipped:

- `selfie spec list`
- `selfie package list`
- `selfie spec validate --all`
- `selfie package audit --all`
- `selfie dotfiles list`
- `selfie apply`

A command that names that one spec fails instead, because the file it was asked about is the file it
cannot read — `selfie spec info ghost`, `selfie package check ghost`, `selfie package status ghost`.

`selfie spec remove ghost` is in that second group, so it will not delete the file. Use `rm`.

### Provider-sourced and templated dotfiles

Where a config file holds a credential, the value can come from a command run at deploy time instead
of being stored in the repository. selfie implements no secret storage of its own — it runs whatever
command you configure, the same way it delegates install and check.

#### A worked template

`packages/rubygems/credentials.tpl`, committed to the repository, contains names and never values:

```
---
:rubygems_api_key: {{ api_key }}
https://gems.internal.corp: Bearer {{ corp_token }}
```

`packages/rubygems.yml`:

```yaml
name: rubygems
environments:
  macos-home:
    install: brew install ruby
dotfiles:
  - source: rubygems/credentials.tpl
    target: ~/.gem/credentials
    vars:
      api_key: op read op://Private/rubygems/token
      corp_token: teller get GEMSERVER_TOKEN
```

The template names values, not stores. Two different tools contribute to one file here, which a
single vendor's own inject command cannot express. Switching to `pass`, `vault`, or `sops` means
changing the right-hand sides; the template is untouched. Because `vars` can appear in an
`environments.<name>.dotfiles` block, the same template can be fed by 1Password on one machine and
something else on another.

#### Substitution rules

- `{{ name }}` is replaced only when `name` is declared in `vars`. Whitespace inside the braces is
  optional.
- Names match `[A-Za-z_][A-Za-z0-9_]*`. A name that does not — `not-a-name`, say — makes the whole
  entry undeployable, because its placeholder could only ever be copied out verbatim. `selfie apply`
  skips the entry **before running any of its commands**, so no credential is fetched for a value
  that could not be used.
- Any other placeholder-like text is left exactly as written, so a file that legitimately contains
  brace syntax passes through unchanged and no escape mechanism is needed.
- Every declared name must appear at least once in the template; an unused name is a validation
  error. This is what catches misspellings — a typo in the template leaves that placeholder literal,
  and the correctly-spelled declared name is then unused.
- Substitution is **single-pass**. A value containing `{{ … }}` is not rescanned.
- There are no conditionals, loops, includes, or expressions. This is substitution, not a templating
  language.

#### Values are substituted verbatim

selfie does not escape a value for the target file's format. A value containing characters
significant to that format can produce a malformed file, and selfie will not detect it.

A value containing a line break is more than a correctness problem: because it is spliced in raw, it
can add structure rather than merely corrupt it — in a credentials file, an extra entry naming a
host you did not configure. Such a value produces a warning naming the binding and is then
substituted as given, since refusing outright would break legitimate multi-line values like private
keys.

#### Working directory and output handling

Both `vars` commands and a whole-file `command` run with their working directory set to the package
file's parent directory — the same base that `source` paths resolve against. They run through a
shell, so pipes, redirection, and `$(…)` are available.

That shell is your **login shell** — `$SHELL -l -c`, falling back to `/bin/sh` when `SHELL` is
unset, and without `-l` on non-Unix. It is the same shell `selfie package install`, `check` and
`audit` use, and the same one the MCP server uses, so a command written in fish or zsh syntax
behaves identically however you invoke it. Because the login profile is sourced, anything your
profile _sets_ rather than exports — `SSH_AUTH_SOCK`, `OP_*` variables, PATH additions — is
available to a provider command.

**Only what your command itself writes to stdout becomes the file's content.** The shell's own
output does not: selfie hands the shell a stdout of `/dev/null` and captures the command's on a
separate descriptor, so a banner from `.zprofile`, a version manager's notice, a background job your
profile started, and anything your profile prints as the shell exits are all discarded rather than
deployed. This holds whether the profile writes before your command, while it is running, or after
it finishes.

Three cases remain, and they are not covered:

- If your **command** installs its own `EXIT` trap (or, under `fish`, exits outright), selfie loses
  the marker it uses to find the end of the output, and whatever the shell prints after the command
  is appended to the content. selfie cannot tell those bytes from the command's, so it deploys them
  and warns that it could not establish where the output ended.
- **A startup file that redirects file descriptor 8 receives your content.** That is the descriptor
  the content travels on. If your `.zprofile` or `.bashrc` does `exec 8>somewhere`, that file gets
  the credential and selfie's own capture comes up empty, so the apply fails and nothing is
  deployed. 8 is chosen to be one nothing much wants — not 3 or 4, the conventional first free
  descriptors, and not 9, which `flock` uses — but if you have taken it, take it back. This is a
  known limit, not a defended boundary: a startup file you did not write can read the deployed
  credential anyway. It is also narrower than the same hazard before this: the content used to
  travel on stdout, where the far more common `exec >somewhere` collected it every time.
- **stderr is not separated at all.** A profile writing to stderr is mixed with the command's, which
  selfie forwards (truncated) only when the command fails.

Keep login-shell output on stderr or guard it on the shell being interactive if you want a quiet
run; you no longer have to in order to get a correct file.

Your command is passed through unchanged, and on every shell but `fish` it is the last thing the
shell is given, so a trailing comment or line continuation is harmless. Under `fish` the command is
run inside a block, so a command whose last line ends in a backslash is refused outright with
`Missing end to balance this begin` rather than deploying anything.

On every shell but `fish`, your command cannot write to the descriptor its own output travels on:
selfie closes it before the command runs. Under `fish` it stays open, but inside the block that
descriptor **is** the command's stdout, so writing to it is writing to stdout — there is nothing
there a command could not already reach.

A command that `cd`s, or a profile that does, still changes the working directory the command ends
up in — that has always been true.

On Windows there is no separation: `cmd.exe` has no login profile to source, and nothing
distinguishes its output from the command's.

Content is written byte for byte, including any trailing newline. `op read` commonly appends one; if
your existing target lacks it you will get a conflict on first apply. Strip it in your own command
if you do not want it.

Zero-length output is an error, for both a whole-file command and an individual binding, regardless
of exit code. Writing an empty file over a credentials target is destructive, and empty output
almost always means a failure that did not set a non-zero status. Whitespace-only output is content,
not empty.

Output selfie could not read to the end is an error too, and nothing is written. A command can run
correctly and still have the pipe carrying its output fail part-way through; what was read by then
is a prefix of the credential, not the credential, and the size limit below cannot catch it because
a truncated value is smaller, not larger. The same applies to an individual binding, where a
truncated value is still non-empty and so clears the zero-length check above as well.

`command_timeout` (default 60s, enough for a biometric prompt) applies **per command**, so an entry
with several bindings can take longer in total.

Resolved content larger than 8 MiB is rejected with an error. Note what that does and does not do:
it bounds what selfie compares and writes, **not** what the command produces. The command's whole
output is buffered before the check can run — as it already is for every install and check command —
so this is not a memory bound against a genuinely unbounded provider.

A failing command respects `stop_on_error`, which defaults to true, so by default a failure aborts
the apply. When a binding fails, the error names that binding and the remaining bindings for that
entry are not run.

Ctrl+C during `selfie apply` cancels a provider command that is still running, as it does during
`install` and `check` — so a command waiting on a biometric or password prompt can be escaped
without waiting out `command_timeout`. The run stops there and reports itself as cancelled; whatever
had already been deployed stays deployed and stays recorded.

#### No deploy state, and what follows from it

Ordinary dotfiles record a checksum of what was deployed, which is how selfie later distinguishes a
changed repository file from a target you edited. Secret-bearing entries record **nothing**: a
stored checksum of a credential is a confirmation oracle — anyone able to read the state file could
test guesses against it offline.

Instead the content is resolved into memory at apply time and compared with the target directly.
Identical content is skipped, an absent target is written, and any difference is a conflict.

Consequences worth knowing before you adopt this:

- A rotated secret produces a conflict rather than refreshing silently.
- Editing a template also produces a conflict, for the same reason.
- selfie cannot tell those two apart from a target you hand-edited, and deliberately does not guess.
- Every apply runs the commands, which may prompt for authentication. Results are never cached: a
  cache would be a secret at rest.
- `selfie dotfiles drift` reports these entries as provider-sourced and unverifiable rather than
  checking them. Checking would mean resolving, which would run your commands from a read-only
  command.

#### Deploy behavior and permissions

Targets are created readable only by their owner (mode `0600` on Unix) and put in place atomically,
so there is no window in which the content is world-readable and no interrupted write can leave a
truncated credential.

A symlink **at the target** is replaced rather than written through: writing through the link would
send the credential wherever the link points. A symlinked **parent directory** is still followed.

Note this differs from a repository-file entry, which is [refused and skipped](#symlinked-targets)
rather than replaced. Neither writes through the link. They differ in what happens next because the
costs differ: a skipped repository file is still in the repository, whereas a skipped credential
leaves you without the file and with nothing recorded about it.

There is one case where whether the link is replaced depends on something other than the deploy
itself. When the content already matches, selfie only rewrites the target if its permissions need
tightening, and the permission check follows the link — so it reports on the file the link points
at. A symlinked target whose destination is already owner-only is left completely alone and the link
survives; one whose destination is group- or world-readable is tightened, which replaces the link
with a regular file. Both outcomes are consistent with the rule above, but which one you get depends
on the destination's mode rather than on anything about the link.

#### What is shown, and what is not

Resolved content never appears in selfie's output — not in a progress message, a log line, an error,
or the MCP server's JSON. A conflict is reported with the target path, the command or template that
produced the content, and a line count for each side:

```
CONFLICT  ~/.gem/credentials
  rubygems/credentials.tpl (vars: api_key, corp_token)
  target exists and differs from resolved output

  resolved output : 3 lines
  current target  : 12 lines
  (content hidden)
```

Line counts are enough to tell a rotated token (1 line vs 1 line) from a hand-edited file (1 line vs
12 lines). Command strings and var names **are** shown: they come from the package file and are
references, not credentials.

At an interactive prompt `selfie apply` offers to reveal the two values, behind its own warning and
its own keypress. It is never the default and is never reachable by accepting one. The MCP server
provides no interactive resolver at all, so it cannot reach that path.

`--yes` / `auto_accept` does **not** apply to these entries. For an ordinary dotfile it forces the
overwrite; for a secret-bearing one the conflict is always reported and skipped, and only an
interactive answer can overwrite. The reason is asymmetric risk: an ordinary target that gets
overwritten wrongly can be recovered from the repository, whereas a credential can not, because
selfie recorded nothing about it. This matters most for non-interactive callers such as the MCP
server, which can set the flag but has no human behind it.

`--dry-run` does not run any provider or `vars` command. That means it cannot tell you whether a
secret-bearing entry would change — knowing that needs the content, and the content needs the
commands. It reports the entry and how many commands it is declining to run. The alternative, a
preview that reaches your secret store and raises a biometric prompt, would make `--dry-run` an
executing operation.

A dry run does still apply every check that can be made without running anything — a target selfie
will not deploy to, whether it is not absolute or names another user's home with `~user/`; a
template escaping the package directory — and reports the same refusal a real apply would. Because
those refusals are failures for a secret-bearing entry, `stop_on_error` (default `true`) ends the
preview at the first one rather than listing every remaining problem entry. That is deliberate: a
preview that continued past an error a real apply would stop on would be describing a different run
from the one you are about to perform. Set `stop_on_error: false` to see them all.

#### Diagnostics do not carry line numbers

Most package validation errors report the YAML line and column they came from. Dotfile entries do
not: their fields are plain strings rather than the span-carrying type the rest of the schema uses,
so a dotfile diagnostic names a field path such as `dotfiles[0].vars` instead of a location. This is
a known gap rather than an intentional design.

#### Limitations

- **A command's stderr is forwarded when it fails.** A provider run with a verbose or debug flag can
  echo secret material to stderr, and that text will appear in the failure message, up to the limit
  described under [Command Output](#command-output). It is not forwarded when the command succeeds.
- **Do not put a literal secret in a var command.** `echo hunter2` stores the secret in the package
  file and exposes it in process listings. A binding should retrieve a value, never contain one.
- **Resolved values are not reliably erasable from process memory.** String and buffer reallocation
  can leave copies behind; selfie makes no scrubbing guarantee.
- **`selfie apply` executes commands from package files**, rather than only copying data out of
  them. `selfie spec validate` reports how many commands a package will run, but treat a package
  directory you did not write as code, not as data.

### Drift Detection

Check whether deployed dotfiles have been modified since they were last deployed:

```bash
# Dedicated drift check (shows per-file status)
selfie dotfiles drift

# Or use apply --dry-run for a preview of what would change
selfie apply --dry-run
```

### Listing Tracked Dotfiles

See all dotfile mappings across packages and standalone dotfiles:

```bash
selfie dotfiles list
```

### Tracking New Files

Start tracking an existing config file so it can be deployed to other machines:

```bash
# Interactive — prompts where to track the file
selfie track ~/.config/starship.toml

# Track as a standalone dotfile (in dotfiles/ directory)
selfie dotfiles track starship ~/.config/starship.toml

# Add to an existing package (in packages/ directory)
selfie package track-dotfile starship ~/.config/starship.toml
```

The track commands copy the file into the repo, create or update the YAML spec with the
source→target mapping, and record initial deploy state for drift detection.

The recorded `target` is `~/…` whenever the file lives under your home directory, whichever form you
passed on the command line — so a spec tracked on one machine works on the next. A file elsewhere is
recorded absolute. The path is also normalized, so `~/.config/../.gemrc` is recorded as `~/.gemrc`.

A target selfie could not deploy to is never recorded: a relative path, or one naming another user's
home with `~user/`, fails with the reason instead of becoming an entry every later `selfie apply`
refuses.

### Validation Rules for Dotfiles

- Exactly one of `source` and `command` — setting both is an error, and so is setting neither
- `source` must not be empty
- `source` must not contain path traversal sequences (`../`)
- `source` must be relative; a path starting with `/` or `~` is an error
- `command` accompanies no `vars`: there is no template to render them into
- Each `vars` name must match `[A-Za-z_][A-Za-z0-9_]*`, or its placeholder would survive into the
  deployed file
- `target` must be an absolute path or start with `~/` — see [Dotfiles](#dotfiles) for which to use
- `target` must not use the `~user/…` form; selfie does not resolve another user's home directory
- A dotfile entry accepts only `source`, `command`, `vars` and `target`

Selfie validates these rules when you run `selfie spec validate`, which reports the line each
problem is on. A package assembled by another tool rather than parsed from a file has no line to
report, and shows `-` instead.

#### Unrecognized keys

Every field of a dotfile entry except `target` is optional, so a misspelled key would otherwise be
dropped without a word. Writing `var:` instead of `vars:` would leave a template looking like an
ordinary repository file and deploy it **unrendered** — with a literal `{{ api_key }}` — over the
target.

An entry carrying a key selfie does not recognize is therefore refused. The same applies to an
anchor inside the entry whose name matches one of the entry's fields, such as `_vars` — see
[YAML anchors](#yaml-anchors) — and to a `vars` name that cannot be substituted, such as
`not-a-name`, which would leave its placeholder in the deployed file:

- `selfie spec validate` reports an error naming the entry and the key, such as `dotfiles[0].var` or
  `environments.macos.dotfiles[1].var`.
- `selfie apply` **refuses that entry** with a warning naming its target, and continues with the
  rest of the package. Only the offending entry is refused; the package's other dotfiles and its
  install and check commands are unaffected. A refused entry makes the run
  [exit non-zero](../README.md#a-refusal-is-not-a-success), so a script cannot mistake it for a
  clean deploy.
- Commands that rewrite the file **refuse to save** it, naming the key: `selfie spec edit`,
  `selfie package track-dotfile`, and the `selfie_spec_update` MCP tool. A rewrite is produced from
  the fields selfie understands, so saving would delete the unrecognized key and quietly turn an
  entry that was being skipped into one that deploys.

  Correct the key by editing the package file in your editor directly. `selfie spec edit` cannot be
  used for this: it saves the package before opening your editor, so on an affected file it refuses
  and exits without opening anything. `selfie spec remove` is unaffected — it deletes the file
  rather than rewriting it.

## YAML anchors

Keys beginning with an underscore are ignored by selfie, so a package file can define YAML anchors
and reuse them with aliases. This works at the top level of the file, inside an individual
environment, and inside an individual dotfile entry, and an underscore-prefixed key is not reported
as an unrecognized key — with one exception, described below: an anchor whose name matches a real
field **at its own level**, such as `_vars` inside an entry, `_check` inside an environment, or
`_dotfiles` at the top level.

```yaml
_brew: &brew brew install ripgrep
_target: &target ~/.config/bat/config

name: ripgrep
environments:
  macos:
    install: *brew
  work-macos:
    install: *brew
dotfiles:
  - source: bat/config
    target: *target
```

Any other unrecognized key is an error — see [Unrecognized keys](#unrecognized-keys). Anchors are a
convenience for writing the file; they are resolved when it is read, and are not preserved if selfie
rewrites the file (for example via `selfie spec edit`).

### An anchor inside an entry may not be named after one of that entry's fields

Inside a dotfile entry, an underscore-prefixed key whose remaining name is `source`, `command`,
`vars` or `target` is an **error**, and the entry is skipped rather than deployed:

```yaml
dotfiles:
  # Refused: is `_vars` an anchor, or a typo for `vars`?
  - source: rubygems/credentials.tpl
    target: ~/.gem/credentials
    _vars: &v
      api_key: op read op://Private/rubygems/token
```

Nothing in the file can tell those two readings apart, and they have opposite consequences. Read as
an anchor, the entry has no `vars` at all, so it is an ordinary repository file — and selfie deploys
the template **unrendered**, with a literal `{{ api_key }}` where the credential belongs, over your
credentials file. That is the same hazard as writing `var:` for `vars:`, which is why it gets the
same treatment: `selfie spec validate` reports it, `selfie apply` skips the entry, and commands that
rewrite the file refuse to save it. See [Unrecognized keys](#unrecognized-keys).

The fix depends on which you meant, and selfie says so rather than guessing:

```
'_vars' cannot be told apart from a misspelling of the 'vars' field; rename it, or correct it to 'vars'
```

**Rename the anchor** — `_creds: &v` works exactly as well — or, if you meant the field, drop the
underscore. Only these four names are affected inside a dotfile entry:

- `_brew`, `_anchor`, `_targets` inside an entry are fine — none of them is a field name.
- `_target: &target …` at the **top level** of the file is fine, as in the example above. Top-level
  keys are a different namespace, and `target` is not a top-level field.

### The same rule applies to a package's top-level keys

A package file accepts `name`, `homepage`, `description`, `dotfiles`, `post_install_note` and
`environments`. Any other key at the top level is refused — a misspelling such as `configs:`, and an
underscore-prefixed anchor whose name is one of those fields (`_dotfiles`, `_name`, …). An anchor
whose name is not a field, such as `_brew:` or `_target:`, is legal and left alone.

```yaml
name: myapp
# Refused: is `_dotfiles` an anchor, or a typo for `dotfiles`?
_dotfiles:
  - source: myapp/config.toml
    target: ~/.config/myapp/config.toml
# Refused: nothing reads `configs`, so these entries would never deploy.
configs:
  - source: myapp/other.toml
    target: ~/.config/myapp/other.toml
```

Both cases bite the same way. The keys selfie reads are not the ones you wrote, so `selfie apply`
has less to deploy than the file appears to describe — and with `_dotfiles:` as the only dotfiles
key, **nothing at all** to deploy, a run that looks completely successful and did nothing.

`selfie apply` therefore **refuses the whole package** and names the key:

```
⚠ Skipping package 'myapp': '_dotfiles' cannot be told apart from a misspelling of the 'dotfiles' field; rename it, or correct it to 'dotfiles'
⚠ Skipping package 'myapp': unknown field 'configs'; expected one of: name, homepage, description, dotfiles, post_install_note, environments
```

The refusal covers the package rather than a single entry, because the problem is in the file's top
level: there is no entry to attach it to, and the entries the file does have may not be the ones you
wrote. It counts as a refusal, so [the run exits non-zero](../README.md#a-refusal-is-not-a-success).
`selfie spec validate` reports the same keys in the same words, and the commands that rewrite a
package file refuse them too — all three agree on the set.

Checking those keys means reading the file a second time, and a file selfie can load as a package
can still fail that second read — a mapping used as a key is one way. `selfie apply` always says so,
and what it does next depends on what the package still has to deploy.

If it has dotfiles for this environment, they are deployed and nothing here makes the run fail.
Nothing is known to be wrong with the file, only unchecked, and what deploys is what selfie read
from the file either way:

```
⚠ Package 'myapp': could not re-read the package file to check its top-level keys, so an unrecognized one -- a misspelling, or an anchor named after a real field -- would not have been caught. Applying it anyway. The re-read failed with: …
```

If it has nothing to deploy, the package is **refused** and
[the run exits non-zero](../README.md#a-refusal-is-not-a-success):

```
⚠ Skipping package 'myapp': it has no dotfiles to deploy, and its top-level keys could not be checked, so a shadowed 'dotfiles:' key cannot be ruled out. The re-read failed with: …
```

Deploying nothing is exactly what `_dotfiles:` looks like from the outside, and this is the one file
selfie cannot check for it — so the two cannot be told apart, and reporting the run as a quiet
success would hide the case this whole section is about. A package that genuinely has no dotfiles is
unaffected, because its keys were read and found clean. `selfie spec validate` reports the failed
read either way, as an advisory notice.

The field lists differ by level, and that is deliberate: `target` is a field of a dotfile _entry_
and not a top-level field, so `_target: &target …` at the top level is an ordinary anchor — the
example above depends on it — while `_target:` inside an entry is refused.

### And to the keys inside an environment

The third list is an environment's own fields — `_install`, `_check`, `_audit`, `_dependencies`,
`_recommends` or `_dotfiles` inside an `environments.<env>` mapping:

```yaml
name: myapp
environments:
  work:
    install: brew install myapp
    # Refused: is `_dotfiles` an anchor, or a typo for `dotfiles`?
    _dotfiles:
      - source: myapp/work.toml
        target: ~/.config/myapp/config.toml
```

Read as an anchor, that environment has **no dotfiles of its own**, so the shared entry deploys on
the work machine instead of the one written for it — the same silence as the top-level case, with
the wrong file in place rather than no file.

`selfie spec validate` reports it as `environments.work._dotfiles`. `selfie apply` refuses the
package too, but only when the environment carrying the key is the one being applied — a key in an
environment this machine does not use cannot affect what it deploys. Run `selfie spec validate`
after editing an environment to catch the ones apply leaves alone.

`_target: &target …` inside an environment is an ordinary anchor, as at the top level: `target` is
not a field of an environment either.

## Common Patterns

### Package Managers

#### Homebrew (macOS)

```yaml
environments:
  macos:
    install: brew install package-name
    check: brew list package-name
```

#### APT (Ubuntu/Debian)

```yaml
environments:
  ubuntu:
    install: |
      sudo apt update \
        && sudo apt install -y package-name
    check: dpkg -l | grep -q package-name
```

#### Pacman (Arch Linux)

```yaml
environments:
  arch:
    install: sudo pacman -S package-name
    check: pacman -Q package-name
```

#### npm (Node.js)

```yaml
environments:
  macos:
    install: npm install -g package-name
    check: npm list -g package-name
    dependencies:
      - node
```

#### pip (Python)

```yaml
environments:
  macos:
    install: pip3 install package-name
    check: pip3 show package-name
    dependencies:
      - python
```

#### Cargo (Rust)

```yaml
environments:
  macos:
    install: cargo install package-name
    check: cargo install --list | grep -q package-name
    dependencies:
      - rust
```

### Direct Downloads

#### GitHub Releases

```yaml
install: |
  set -e
  LATEST=$(curl -s https://api.github.com/repos/owner/repo/releases/latest | jq -r .tag_name)
  curl -Lo binary https://github.com/owner/repo/releases/download/${LATEST}/binary-linux-amd64
  sudo install binary /usr/local/bin/
```

#### Installer Scripts

```yaml
install: |
  set -e
  curl -fsSL https://get.example.com | bash
  source ~/.bashrc
```

#### Container Images

```yaml
install: docker pull alpine:latest
check: docker images | grep -q alpine
dependencies:
  - docker
```

### Language Runtimes

#### Node.js with Version Management

```yaml
environments:
  macos:
    install: |
      set -e

      # Install nvm if not present
      if ! command -v nvm &> /dev/null; then
        curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
        source ~/.bashrc
      fi

      nvm install 20
      nvm use 20
      nvm alias default 20
    check: node --version | grep -q "v20"
```

#### Python with Virtual Environment

```yaml
install: |
  set -e
  python3 -m venv ~/.virtualenvs/myproject
  source ~/.virtualenvs/myproject/bin/activate
  pip install -r requirements.txt
check: |
  set -e
  source ~/.virtualenvs/myproject/bin/activate
  python -c "import required_package"
```

## Validation Rules

Selfie validates package files according to these rules:

### Required Fields

- `name` must be present and match filename
- `environments` must contain at least one environment
- Each environment must have an `install` command

### Naming Conventions

- Package names should be lowercase with hyphens (kebab-case)
- Environment names should be descriptive and consistent
- Avoid special characters in names

### Dependencies

- Dependencies must reference valid package names
- Circular dependencies are not allowed
- Dependencies should exist in the package directory

### Commands

- Install and check commands should be valid shell scripts
- Multi-line commands should consider including `set -e` for proper error handling (stops execution
  on first failure)
- Commands should handle errors appropriately and provide meaningful error messages
- Long-running commands should provide progress feedback

## Best Practices

### 1. Make Commands Idempotent

Ensure install commands can be run multiple times safely:

```yaml
install: |
  if ! command -v tool &> /dev/null; then
    curl -Lo tool https://example.com/tool
    sudo install tool /usr/local/bin/
  fi
```

### 2. Provide Clear Error Messages

Help users understand what went wrong:

```yaml
install: |
  if ! command -v curl &> /dev/null; then
    echo "Error: curl is required but not installed"
    exit 1
  fi
  # ... rest of installation
```

### 3. Use Specific Versions

Pin to specific versions for reproducibility:

```yaml
install: |
  VERSION=1.21.5
  curl -Lo go.tar.gz https://go.dev/dl/go${VERSION}.linux-amd64.tar.gz
  sudo tar -C /usr/local -xzf go.tar.gz
```

### 4. Use External Scripts for Complex Installations

For complex multi-step installations, consider using external shell scripts. Commands automatically
run in the package directory, so you can reference scripts using relative paths:

```yaml
environments:
  ubuntu:
    install: ./my-package/ubuntu_install.sh
    check: ./my-package/check.sh

  macos:
    install: |
      # Multi-line scripts also run in package directory
      source ./my-package/common.sh
      ./my-package/macos_install.sh
    check: ./my-package/check.sh
```

**Benefits of external scripts:**

- Better code organization and reusability
- IDE syntax highlighting and linting
- Easier testing and debugging
- Version control friendly
- Shared utilities across environments

**Example structure:**

```
package-directory/
├── my-package.yaml        # Package definition (must be at root)
├── other-package.yaml     # Other packages also at root
└── my-package/            # Scripts directory named after package
    ├── common.sh          # Shared utilities
    ├── ubuntu_install.sh  # Ubuntu-specific installation
    ├── macos_install.sh   # macOS-specific installation
    └── check.sh           # Shared verification script
```

**Script path approaches:**

```yaml
# Recommended: Use relative paths (commands run in package directory)
install: ./my-package/ubuntu_install.sh

# Alternative: Multi-line with relative paths
install: |
  set -e
  source ./my-package/common.sh
  ./my-package/ubuntu_install.sh

# Absolute paths work too of, course
install: /absolute/path/to/packages/my-package/ubuntu_install.sh
```

**Script patterns:**

```bash
#!/bin/bash
set -e

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

log_info "Installing my-package..."
# Installation logic here
verify_installation
```

See `docs/examples/docker-scripted.yaml` and `docs/examples/docker-scripted/` for a complete
example.

### 4. Handle Different Architectures

Account for different CPU architectures:

```yaml
install: |
  ARCH=$(uname -m)
  case $ARCH in
    x86_64) ARCH=amd64 ;;
    aarch64) ARCH=arm64 ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
  esac
  curl -Lo binary https://example.com/binary-linux-${ARCH}
```

### 5. Cleanup After Installation

Remove temporary files:

```yaml
install: |
  cd /tmp
  curl -Lo installer.tar.gz https://example.com/installer.tar.gz
  tar xzf installer.tar.gz
  sudo ./install.sh
  rm -rf installer.tar.gz install.sh
```

### 6. Verify Installation

Add verification steps to install commands:

```yaml
install: |
  brew install package-name
  if ! command -v package-name &> /dev/null; then
    echo "Installation failed: package-name not found in PATH"
    exit 1
  fi
  echo "Successfully installed package-name"
```

## Troubleshooting

### Common Issues

#### Package Not Found

```
Error: Package 'package-name' not found
```

- Ensure the package file exists in your package directory
- Check that the filename matches the package name
- Verify the file has a `.yaml` extension

#### Validation Errors

```
Error: Package validation failed
```

- Run `selfie spec validate package-name` for detailed errors
- Check YAML syntax with a YAML validator
- Ensure all required fields are present

#### Installation Failures

```
Error: Installation command failed
```

- Run with `--verbose` flag for detailed output
- Check that all dependencies are installed
- Verify commands work in your shell manually
- Ensure you have necessary permissions

#### Check Command Failures

```
Warning: Check command failed
```

- Verify the check command syntax
- Test the check command manually
- Ensure the check command matches the actual installation location

### Debugging Tips

1. **Use verbose mode**: `selfie --verbose package install package-name`
2. **Test commands manually**: Run install/check commands in your shell
3. **Check dependencies**: Ensure all dependencies are properly installed
4. **Validate syntax**: Use `selfie spec validate package-name`
5. **Check permissions**: Ensure you have necessary permissions for installation paths
6. **Review logs**: Check system logs for additional error information

## Examples

See the [examples directory](examples/) for complete package definitions covering:

- Popular development tools
- Language runtimes
- Container platforms
- Cloud CLI tools
- Text editors and IDEs
- System utilities

## Migration from Other Tools

### From Homebrew Bundle

Convert `Brewfile` entries:

```ruby
# Brewfile
brew "ripgrep"
cask "docker"
```

```yaml
# ripgrep.yaml
name: ripgrep
environments:
  macos:
    install: brew install ripgrep
    check: which rg
```

### From package.json

Convert global npm packages:

```json
{
  "devDependencies": {
    "typescript": "^5.0.0"
  }
}
```

```yaml
# typescript.yaml
name: typescript
environments:
  macos:
    install: npm install -g typescript@5.0.0
    check: which tsc
    dependencies: [node]
```

### From requirements.txt

Convert Python packages:

```
black==23.0.0
flake8==6.0.0
```

```yaml
# black.yaml
name: black
environments:
  macos:
    install: pip install black==23.0.0
    check: which black
    dependencies: [python]
```
