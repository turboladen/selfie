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

## Limitations

- **No merge conflict resolution** — if the remote has diverged, you need to resolve manually with
  git.
- **No multi-repo sync** — `package_directory` and `dotfiles_directory` must be in the same
  repository.
- **No network auth management** — configure SSH keys or credential helpers yourself.
