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

The package name, which must match the filename (without `.yaml` extension).

```yaml
name: ripgrep # for file ripgrep.yaml
```

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

Every entry has a `target` — the absolute destination path, supporting `~` for the home directory —
plus one of three ways of saying where the content comes from.

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

Any other key inside a dotfile entry is a parse error. Only `target` is required, so a misspelling
would otherwise be dropped silently — writing `var:` for `vars:` would leave a valid-looking
repository-file entry and deploy the template _unrendered_, placeholders and all, over the file it
was meant to fill in.

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

Each environment must have an `install` command and optionally a `check` command, `dependencies`,
and `recommends`.

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
- Names match `[A-Za-z_][A-Za-z0-9_]*`.
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

Content is written byte for byte, including any trailing newline. `op read` commonly appends one; if
your existing target lacks it you will get a conflict on first apply. Strip it in your own command
if you do not want it.

Zero-length output is an error, for both a whole-file command and an individual binding, regardless
of exit code. Writing an empty file over a credentials target is destructive, and empty output
almost always means a failure that did not set a non-zero status. Whitespace-only output is content,
not empty.

`command_timeout` (default 60s, enough for a biometric prompt) applies **per command**, so an entry
with several bindings can take longer in total.

Resolved content larger than 8 MiB is rejected with an error. Note what that does and does not do:
it bounds what selfie compares and writes, **not** what the command produces. The command's whole
output is buffered before the check can run — as it already is for every install and check command —
so this is not a memory bound against a genuinely unbounded provider.

A failing command respects `stop_on_error`, which defaults to true, so by default a failure aborts
the apply. When a binding fails, the error names that binding and the remaining bindings for that
entry are not run.

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

A symlink **at the target** is replaced rather than written through. This is a deliberate behavior
change: writing through the link would send the credential wherever the link points. A symlinked
**parent directory** is still followed.

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

A dry run does still apply every check that can be made without running anything — a target that is
not absolute, a template escaping the package directory — and reports the same refusal a real apply
would. Because those refusals are failures, `stop_on_error` (default `true`) ends the preview at the
first one rather than listing every remaining problem entry. That is deliberate: a preview that
continued past an error a real apply would stop on would be describing a different run from the one
you are about to perform. Set `stop_on_error: false` to see them all.

#### Diagnostics do not carry line numbers

Most package validation errors report the YAML line and column they came from. Dotfile entries do
not: their fields are plain strings rather than the span-carrying type the rest of the schema uses,
so a dotfile diagnostic names a field path such as `dotfiles[0].vars` instead of a location. This is
a known gap rather than an intentional design.

#### Limitations

- **A command's stderr is forwarded when it fails.** A provider run with a verbose or debug flag can
  echo secret material to stderr, and that text will appear in the failure message. It is not
  forwarded when the command succeeds.
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

### Validation Rules for Dotfiles

- `source` must not be empty
- `source` must not contain path traversal sequences (`../`)
- `target` must be an absolute path (or start with `~`)

Selfie validates these rules when you run `selfie spec validate`.

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
