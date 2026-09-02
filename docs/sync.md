# Git Sync

Selfie can sync your package specs and dotfiles across machines using git. The `selfie sync`
commands are thin wrappers around git that generate useful per-package commits automatically.

## Prerequisites

Your package directory must be inside a git repository with a configured remote. Selfie discovers
the repo by walking up from `package_directory`.

```bash
cd ~/selfie-packages
git init
git remote add origin git@github.com:you/selfie-packages.git
```

If `dotfiles_directory` is a sibling inside the same repo (the recommended layout), it gets synced
automatically.

## Commands

### `selfie sync status`

Shows combined repo status and dotfile drift:

```
✓ Repository: ~/selfie-packages (main)
  No uncommitted changes, up to date with remote

✓ No dotfile drift (5 deployed)
```

When changes exist:

```
ℹ Repository: ~/selfie-packages (main)
  2 modified, 1 untracked
  Up to date with remote

⚠ Dotfile drift: 1 drifted out of 5 deployed
  ~/.config/starship.toml
  → Run 'selfie apply' to redeploy or 'selfie dotfiles drift' for details
```

This summary reports counts and target paths only. When a target keeps reappearing here and
`selfie apply` never clears it, run `selfie dotfiles drift` — it explains why, and a
[symlinked target](package-files.md#symlinked-targets) is the usual cause. `sync status` does not
carry that reason itself.

### `selfie sync push`

Groups changed files by package and generates conventional commit messages:

```bash
selfie sync push          # Per-package commits, prompts for each message
selfie sync push -y       # Accept generated messages without prompting
selfie sync push --batch  # Single commit for all changes
selfie sync push --batch --message "update all specs"
selfie sync push --include-ungrouped  # Include non-package files
```

**File grouping rules:**

- YAML files (`*.yml` / `*.yaml`) -> package name is the file stem (`starship.yml` -> `starship`)
- Files in a subdirectory -> package name is the directory name (`starship/starship.toml` ->
  `starship`)
- Files that don't match either pattern are "ungrouped" and skipped unless `--include-ungrouped` is
  used

**Conventional commit messages** are generated automatically:

| Change                | Message                                     |
| --------------------- | ------------------------------------------- |
| New YAML file         | `feat(starship): add package spec`          |
| Modified YAML         | `chore(starship): update package spec`      |
| Deleted YAML          | `chore(old-tool): remove package spec`      |
| Dotfile source only   | `chore(starship): update dotfile`           |
| Both YAML and dotfile | `chore(starship): update spec and dotfiles` |

When prompting, the generated message is the default — press Enter to accept or type a new message.

**Flags:**

| Flag                     | Effect                                 |
| ------------------------ | -------------------------------------- |
| `--batch`                | Single commit for all changes          |
| `--message "..."` / `-m` | Override message (only with `--batch`) |
| `--yes` / `-y`           | Accept all generated messages          |
| `--include-ungrouped`    | Include non-package files              |

### `selfie sync pull`

Fetches and fast-forward merges from the remote:

```bash
selfie sync pull
```

Refuses if there are staged changes (indicating an in-progress commit). Modified and untracked files
are allowed through — `git merge --ff-only` fails safely if they'd conflict. After pulling, shows
what changed:

```
✓ Pulled 3 commits from remote
  updated: starship, alacritty
  added: fnm
```

If dotfile source files changed, suggests redeploying:

```
⚠ Dotfile sources changed — run 'selfie apply' to deploy updates
```

## Typical Workflow

```bash
# On machine A: make changes, push
selfie sync status        # See what changed
selfie sync push -y       # Commit and push

# On machine B: pull and apply
selfie sync pull          # Get latest specs
selfie apply              # Deploy updated dotfiles
```

## MCP Tools

The MCP server exposes three sync tools for AI assistant integration:

| Tool                 | Parameters                                          | Notes                   |
| -------------------- | --------------------------------------------------- | ----------------------- |
| `selfie_sync_status` | none                                                | Returns structured JSON |
| `selfie_sync_push`   | `batch`, `message`, `messages`, `include_ungrouped` | Per-package by default  |
| `selfie_sync_pull`   | none                                                | Returns what changed    |

The `messages` parameter on `selfie_sync_push` is a map of package name to custom commit message,
allowing AI assistants to provide meaningful messages without interactive prompting.

## Two files claiming one package name

`selfie sync push` refuses to push a package directory holding two specs that resolve to the same
package name. A name is the filename without its extension, compared ignoring case, so all of these
are one package:

- `Neovim.yml` and `neovim.yml` — the capitalization differs
- `neovim.yml` and `neovim.yaml` — the extension differs
- `Neovim.YML` and `neovim.yaml` — both differ

Rename or remove all but one before pushing. The refusal says which files collided.

Both flavors leave the package unresolvable, and the first also destroys a file: two capitalizations
of one filename cannot both survive a checkout on a case-insensitive file system, so a clone there
keeps one and discards the other with no diagnostic. Two extensions both survive. The refusals say
different things for that reason.

## Limitations

- **No merge conflict resolution** — if the remote has diverged, you need to resolve manually with
  git.
- **No multi-repo sync** — `package_directory` and `dotfiles_directory` must be in the same
  repository.
- **No network auth management** — configure SSH keys or credential helpers yourself.

## Credentials in git error messages

When a git operation fails, selfie forwards git's own stderr into the error it reports — to the
terminal, and to an AI assistant through the sync MCP tools. Before that happens, the **userinfo
component of the URLs it recognizes is replaced with `***`**, and the message is truncated if it is
very long.

This matters because a remote URL can carry a credential. `https://<token>@github.com/you/repo.git`
is what `gh auth setup-git` writes, and a git that cannot prompt for a password — which is how the
MCP server runs it — names that URL in the failure:

```text
fatal: could not read Password for 'http://***@github.com': terminal prompts disabled
```

Both halves of the userinfo go, not just the password: a personal access token is usually the
_username_. Redaction also applies to SSH-style remotes, so `git@github.com` reads as
`***@github.com` in an error, and to any address in git's "please tell me who you are" message.

**It covers URLs, not credentials in general.** A token echoed outside a URL — an `Authorization`
header in a `GIT_TRACE` dump, or output from a credential helper — is not redacted, because there is
no reliable way to recognize one.

**And it recognizes URLs by position, not by parsing.** A credential is found at the start of a
whitespace-delimited word, after `://`, and after `=` or `,` — which covers ordinary remotes,
SSH-style remotes, and the `url.<base>.insteadOf=<url>` form used to inject one. It is **not** found
after a path separator, so a URL buried inside another URL's path is left alone; treating every `/`
as a boundary would mangle the host out of ordinary messages, which costs more than it saves.

Treat selfie's error output as you would git's own: safer than raw, not a guarantee.
