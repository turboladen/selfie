# 0012. Which spec files are packages is decided once, from the files' names

Date: 2026-10-25

## Status

Accepted 2026-10-25. Refines [0009](0009-a-status-selfie-cannot-judge-names-why-in-one-type.md),
whose lookup classification it feeds, and
[0010](0010-a-package-selfie-could-not-answer-for-makes-the-run-found.md), whose "two files claim
the name" reason it makes reachable from every listing.
[0013](0013-two-entries-are-one-target-when-their-expanded-targets-are-equal.md) runs its cross-spec
collision scan over the collection this record defines.

## Context

A package is named by its spec file: the file's stem with case folded, with `.yml` and `.yaml`
interchangeable. Specs live in two directories, the package directory and the dotfiles directory,
which may be configured as one. From those facts follow three situations that need a ruling:

- **Several files in one directory claim one name**, such as `bat.yml` and `bat.yaml`, or `Bat.yml`
  and `bat.yml`. No command can tell which one the user means, and a case-insensitive checkout keeps
  only one of the second pair.
- **The dotfiles directory has a spec under a name the package directory claims.** One of the two
  has to win, or a deploy writes both.
- **The two settings name one directory** under different spellings. Reading it twice lists every
  spec twice and makes every name collide with itself.

The rulings exist, but each reader of a spec directory applies them itself:

- The dotfile collection behind apply, drift and `spec validate --all` refuses an ambiguous name and
  sets aside a shadowed dotfiles spec.
- The named lookup behind install, status, check, audit, info, update, remove and track into a
  package refuses an ambiguous name with an error of its own.
- The listings see every file: `package list`, `spec list`, `spec search`, `audit --all`, the
  dependents lookup before a removal, the interactive pickers that offer package names, and
  `dotfiles list`.
- Sync push reads the working tree itself and re-implements the claims and the shadowing.

So one directory gets several answers: `package list` shows `bat` twice and runs both checks,
`install bat` refuses it, and `audit --all` reports two audit errors.

selfie never makes such a pair itself: creating, editing and tracking a spec, from the CLI or the
MCP server, refuse a name a spec file already claims. A pair arrives only when a file is added by
hand or by a `git pull`, so it is caught where specs are read, and the refusal names every file
claiming the name and tells the user to rename or remove all but one.

Collection also lives in the dotfile service and speaks its vocabulary, although the package service
and sync push need its answer. Its refusals carry no facts of their own: two of them rely on a
warning sent separately, so a refusal added without its warning compiles and loses a message.

Separately, a spec file's `name:` field is shown by every listing and decides nothing. A user who
reads `neovim` in `package list` and runs `selfie apply neovim` is told no such package exists.

## Decision

### 1. One table of names, from the file names alone

One function in the package module takes the file names each directory lists, and whether the two
directories are one, and returns what each folded name means:

- **one file** claims it, in the package directory or the dotfiles directory;
- **several files** in one directory claim it: an ambiguous name, carrying the files, sorted;
- **set aside**: a dotfiles-directory file under a name the package directory claims, however many
  package files claim it and whether or not they parse.

It reads no file and parses nothing, so it gives the same answer for a directory whatever its specs
hold. Loading the specs comes after, and attaches each parse result to its file.

Every reader that decides what a name means asks this table: the collection for apply, drift,
`dotfiles list` and `spec validate --all`; the package listings; the interactive pickers; the named
lookup's ambiguity; and sync push, over the listing of its working tree. Push keeps a listing of its
own, since what it validates is the tree it is about to commit. The table has two halves. The
per-directory half, which files claim one name, is attached to a repository's listing, which keeps
ambiguous files as a group of their own carrying the table's answer rather than dropping them; so
every caller of the listing receives it. The cross-directory half, what a set-aside spec is, is
asked by collection and by push. The check that a new name is free needs nothing from the table: any
file claiming the name makes it taken.

What a command does with each answer is its own verdict, decided from the one classification:

| command                                    | an ambiguous name                                                                                                                | a set-aside dotfiles spec               | verdict for the ambiguity                             |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------- | ----------------------------------------------------- |
| `apply`, `dotfiles drift`                  | refused and counted when any of its files deploys here or does not parse; otherwise not mentioned                                | warned about, not deployed              | Failed, as any refusal                                |
| `apply <name>` and every named command     | fails as ambiguous, naming the files                                                                                             | the package spec is the one used        | Failed                                                |
| `spec validate --all`                      | an error                                                                                                                         | warned about, not validated             | Failed                                                |
| `package list`, `spec list`, `spec search` | each file that parses is a refused row of kind `ambiguous_name`, naming every file; one that does not parse stays an invalid row | not a package; not shown                | `package list` Found, exit 3 (0010); the others Clean |
| `package audit --all`                      | refused once, counted as refused; no audit runs                                                                                  | not a package; not shown                | Failed, as a spec left out is (0006)                  |
| the dependents lookup                      | its files are reported among the specs selfie could not rely on, and still checked                                               | not a package                           | unchanged                                             |
| the interactive pickers                    | not offered                                                                                                                      | not offered                             | not applicable                                        |
| `dotfiles list`                            | each file a refused row, as in `package list`                                                                                    | not listed; apply's warning is printed  | Clean                                                 |
| `sync push`                                | refused, whether or not it deploys anything                                                                                      | not validated, as apply does not use it | Failed                                                |

`spec list`, `spec search` and `dotfiles list` already show other refused specs as rows and stay
Clean, which 0006 decides for every listing but `package list`. An ambiguous name is one more
refused row, so their verdict does not change.

Apply does not mention an install-only ambiguity because neither file deploys anything here, and
every command that would use one refuses the name itself. Push refuses every one because the machine
that pulls cannot resolve the name, and a pair differing only in case loses a file on a
case-insensitive checkout.

Rejected: detect ambiguity inside the repository's listing and drop the files from its output. It
hides the files from the listings that exist to show them, and the repository would be deciding a
verdict that differs per command. Rejected: leave each reader its own rule and share only the
function that folds a file name. Folding is the easy half; what a collision or a shadowing means is
the part each reader got differently.

### 2. Collection is the package module's, and its findings carry themselves

Collection belongs to the package module, named for what it does rather than for apply. It returns
the collected packages and one list of findings in the order it found them. A finding is a warning
or a refusal, and each refusal carries the facts its sentence and its event need: the listing error
for a dotfiles directory selfie could not read, the parse error for a spec that could not be loaded,
the files for an ambiguous name, and for those last two the directory the files were found in. Each
one sends its own event, so a refusal cannot exist without its message.

A spec in the dotfiles directory that does not parse, under a name the package directory claims,
stays a warning: nothing would have used it. The findings leave in the order they were found, so a
skipped spec's warning still precedes the failure it explains. A run asked for one name sends the
findings about that name and about no name, and counts none of them; a run over everything sends and
counts them all.

Rejected: a warning list beside a refusal list, paired by convention. A count or a message is lost
whenever one side is added without the other, and nothing fails to compile.

### 3. One directory is one device and inode

Whether the package directory and the dotfiles directory are one directory is answered by the file
system port, as whether the two paths have the same device and inode. It is asked only once both
paths are known to be directories. A directory that cannot be examined is not shown to be the other
one, so both are read. Sync push asks the same question to decide whether its dotfiles directory is
its package directory.

Rejected: comparing canonical paths. They miss aliases canonicalization does not unify, such as a
macOS firmlink (`/System/Volumes/Data/Users/me/...` beside `/Users/me/...`) and a Linux bind mount.
The shared directory is then read twice, every spec's name collides with itself, and the
standalone-track refusal for a shared directory is skipped.

### 4. A name only a `name:` field answers to

Package identity stays the file name. A lookup that finds no file for a name, and finds specs whose
`name:` field folds to it, says so in its failure: "No package named 'neovim' was found. nvim.yml
declares `name: neovim`; a package is named by its file, so run `selfie apply nvim`."

There are two not-found answers, and both carry the hint. A named apply or drift answers from the
packages it already collected from both directories. The named lookup behind every other command
answers from the package directory: on that failure path it parses the specs it listed, which it
otherwise does not. The reason labels stay `not_found`; the hint is in the sentence.

Rejected: treat `name:` as an alias. A package would have two names, and a file named `neovim.yml`
beside an `nvim.yml` declaring `name: neovim` would make the lookup ambiguous across files that do
not collide by any rule the table knows. Rejected: a structured `suggestions` field in MCP failure
rows. Nothing branches on it, and the sentence already reaches an assistant through `error`.

## Consequences

- `package list`, `spec list` and `spec search` stop showing an ambiguous name as packages. Each
  file that parses appears among the refused rows instead, with kind `ambiguous_name`, so the MCP
  row shape is unchanged. A `package list` over such a directory exits 3 until the user removes a
  file.
- `package audit --all` reports an ambiguous name once, as a refusal, instead of an error per file,
  and audits every package by its spec name, so a spec whose `name:` differs from its file name is
  audited rather than reported as an error.
- The interactive pickers stop offering a name no command can use.
- `dotfiles list` lists what apply would use and prints apply's warnings, instead of every file.
- Sync push reads each spec directory once per run, and agrees with apply about which files it
  validates and which names it refuses, by construction.
- The dotfile service exports no collection; its warning type holds only what apply raises.
- The file system port gains a question about whether two directories are one; the repository's
  canonicalized-directory method goes. On a file system that does not report stable inode numbers,
  such as some FUSE, virtiofs and NFS mounts, two directories can share an identity and be taken as
  one.
- A not-found failure in any named command may carry the hint in §4.

Reopen when a spec directory can hold specs in subdirectories, since the table assumes one level; or
when a third spec directory appears; or when `name:` is made to mean something, at which point §4's
hint becomes a lookup.
