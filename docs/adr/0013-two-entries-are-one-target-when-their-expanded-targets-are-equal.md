# 0013. Two entries are one target when their expanded targets are equal

Date: 2026-10-25

## Status

Accepted 2026-10-25. Refines [0001](0001-machine-specific-and-secret-dotfiles.md): an environment
entry replaces a shared entry with the same expanded target rather than the same spelling, and
validation reports every override as an informational note, including one it can confirm only on a
particular home, except one reached through a `..` that climbs out of home. Refines
[0005](0005-directory-and-target-state.md) §5: it extends that section's rule for two packages to a
package and a standalone dotfile spec, replaces its "warns once ... counts the target as refused"
with one refusal per entry, and replaces its paragraph on the scope of track's scan. Refines
[0008](0008-a-dotfile-entry-is-refused-or-failed-and-its-kind-names-the-condition.md), whose
`Collision` condition the cross-spec refusal uses. Uses the collection decided in
[0012](0012-which-spec-files-are-packages-is-decided-once-from-the-files-names.md).

## Context

selfie compares dotfile targets in four places, and they do not compare the same way.

- **Collisions inside one package** compare targets after expanding `~` against the home directory
  and resolving `.` and `..` as text. They are never canonicalized: a target reaches its writer
  unresolved, so a symlink at it is seen.
- **An environment override** replaces a shared entry only when the two targets are spelled
  identically. A shared `~/.config/x` and a `work` entry `/Users/me/.config/x` are meant as an
  override, and apply in `work` refuses both as a collision, telling the user to respell one. Track
  produces exactly this pair: tracking a file in one environment appends a shared entry, however
  another environment's entry for the same file is spelled.
- **Across specs** nothing compares. Two packages, or a package and a standalone dotfile spec, can
  each deploy to one file; apply writes both, the target flips between them, and the deploy state's
  checksum flips with it.
- **Validation** has no home directory, so it compares targets with `~` left as written. Its key has
  to keep a `..` it cannot resolve: one with nothing before it, or one that would remove the `~`,
  says where the file is in terms only a home directory can settle. A key that dropped it would make
  `~/../a/.x` and `~/../../a/.x` one target, though they are two files on every machine, and because
  `sync push` validates what it pushes, a false collision would refuse a push.

Track adds a fifth question. Before it adds an entry, it asks whether the file is already tracked,
and it asks only the spec it is writing to. `selfie dotfiles track <new-name> <file>` for a file a
package already deploys creates a second spec for one file.

## Decision

### 1. One identity

Two entries are one target when their targets are equal after `~` is expanded against the run's home
directory and `.` and `..` are resolved as text. Nothing is canonicalized. The same identity decides
collisions inside a package, overrides, collisions across specs, and whether track finds the file
already tracked.

### 2. An override is matched by identity

An environment entry overrides every shared entry with the same identity, however either is spelled.
So `~/.x` and `/Users/me/.x` are an override on a machine whose home is `/Users/me`, and two
unrelated entries on a machine whose home is elsewhere, where both deploy.

Validation has no home, and must not have one: it judges a file that will be applied on machines
with different homes, and its verdict cannot depend on which one runs it. It compares a lexical key:
the target as written, with `.` and `..` resolved where they can be, and a `..` kept where nothing
before it can be removed. A leading `~` is never removed by a `..`. Two targets with equal keys name
one file on every machine, so validation never reports a pair that apply would treat as two files,
and it reports an override it can confirm. A confirmed override is reported as an informational
note, not a warning: overrides are an intended feature, and a warning fails `spec validate` and
refuses `sync push`.

A pair whose keys differ can still be one file on some machine: one side has an absolute target and
the other a `~` target whose path below home is a suffix of it, whichever side is the shared entry.
Validation reports that pair as an informational note naming the home on which it would override:
"`environments.work.dotfiles[0]` (`/Users/me/.x`) overrides the shared `~/.x` on a machine whose
home is `/Users/me`." It is informational because whether it overrides depends on the machine, not
on the file. The note is given only when that home is where a home directory conventionally is:
`/root`, or a directory directly in `/Users`, `/home` or `/var/home`. A pair implying any other
home, such as `/usr/local/bin/tool` and `~/bin/tool`, gets no note, and apply still treats it
correctly on whatever machine it runs. A pair that meets only after a `..` climbs out of home and
back in, such as a shared `~/../me/.x` and an environment `~/.x`, is not reported, and is a known
cost (§5).

Rejected: keep spelling as the override rule and teach track to copy the other entry's spelling. It
leaves a hand-written pair refused for a reason the user cannot see in the file. Rejected: give
validation the current machine's home. A push from one machine would then pass or fail depending on
where it ran. Rejected: say nothing about a pair validation cannot confirm. Apply would then replace
a shared entry with nothing in validation's output saying so, which is what 0001 forbids.

### 3. Across specs, neither entry deploys

When entries of two or more specs, packages or standalone, have one identity in the run's
environment, none of them deploys. Each entry is refused, with condition `Collision` at `Entry`, in
a sentence naming every spec and entry that deploys there. Drift refuses the same entries. A run
asked for one package refuses that package's entry too, since deploying it would overwrite the other
spec's file. Each refused entry counts once, as a collision inside a package already counts, so two
packages sharing one file report two refusals, one per entry, and two `dotfile_refused` rows.

Which entries take part:

- Only specs the collection uses. A spec whose name is ambiguous or set aside deploys nothing.
- Not a package refused whole, for an unknown key or for declaring no environment. Nothing of it
  deploys, and its entries may not be the ones it means.
- An entry refused for a reason of its own, such as the target rule or a source that escapes its
  directory, still takes part. A collision is checked first inside a package for the same reason:
  every entry sharing the target gets the same answer whatever else is wrong with one of them.

There is no precedence between specs, and none between a package and a standalone spec. Any
tie-break other than refusing is an order the user did not choose, and a deploy state that records
one source per target would flip between them.

Rejected: one refusal per target, as 0005 §5 worded it. A consumer counting refused entries would
see fewer than the entries that did not deploy, and the count would differ from a collision inside
one package. Rejected: let a standalone spec take over a package's target. It is a precedence by
directory, and a user moving a file from one to the other would get different results depending on
which copy they deleted first. Rejected: refuse the whole run. It blocks an apply of every unrelated
package over one shared file.

Amended 2026-10-26: what collection refused takes no part either, because its entries are unknown,
so apply and drift carry on and say what they could not compare. A run over everything names every
spec file, or the dotfiles directory, it could not use in one warning, after each one's own: as
`packages/<file>` or `dotfiles/<file>`, every file of an ambiguous name included. A run asked for
one package names each with why it could not be used. A run with no entry in this environment says
nothing, and nor does one `stop_on_error` stopped before it deployed. Rejected: refuse every entry
while any spec is unreadable. As §4 says of track, an unrelated broken spec would block every apply.

### 4. Track asks the same question of every spec

Within the spec it writes to, track counts an entry as already tracking the file when the entry
deploys in the current environment. Another environment's entry with the same identity overrides the
shared entry track adds, so it is no collision.

Across specs, track counts every scope of every other collected spec, because nothing across specs
overrides. When another spec deploys the file anywhere, the track is refused, naming that spec and
the command that tracks into it. The interactive tracker asks the same library function, so it and
the explicit track commands find the same trackers. A spec selfie could not read may also track the
file; track names each one it could not check and goes on.

Rejected: answer "already tracked" with success when another spec tracks the file. The user named a
spec, and nothing was added to it. Rejected: refuse a track because an unrelated spec does not
parse. The apply-time refusal in §3 remains the backstop.

### 5. Known costs

Identity is lexical, so it misses what only the file system knows: two spellings differing in case
on a case-insensitive volume, a path through a symlinked parent directory, and a firmlink or bind
mount. Two specs deploying `~/.Gemrc` and `~/.gemrc` on macOS still flip one file between them.
These are accepted. So is an override validation cannot see, reached through a `..` that climbs out
of home and back in: apply honors it, and validation says nothing about it.

Resolving targets for comparison would catch them, and the orphan check already does it, comparing
the result and dropping it so nothing is written through it. A collision refusal cannot lean on it
the same way. Resolution works only for targets that exist, so a pair would deploy on the first run
and be refused on the second. A refusal that turns on what is on disk tells the user something
different from one machine to the next, where an orphan report only stays silent.

## Consequences

- Apply and drift refuse cross-spec collisions. A user with two specs deploying one file sees both
  entries refused, a refused count of 2, and a non-zero exit until one is removed.
- An environment entry spelled differently from the shared entry it means to override stops being
  refused. `docs/package-files.md` drops "an override has to spell its target exactly".
- `spec validate` reports overrides as informational notes, including one it can only confirm on one
  home, so a spec using an override can be pushed. An override reached through `..` out of home goes
  unreported. Validation also stops reporting collisions for targets that climb above `~`
  differently, so a push it blocked for that reason goes through.
- `dotfiles track` and `package track-dotfile` refuse a file another spec deploys, and the
  interactive tracker's own scan moves into the library.
- 0005 §5's paragraph saying track's scan reads every scope is replaced by §4: every scope of other
  specs, the current environment of its own.
- The effective set of a package's entries depends on the home directory, so computing it takes one.
  Callers without a home use validation's lexical key.

Reopen when the deploy state can hold more than one source per target; when entries gain an order or
a precedence; or when a user reports a case-only or symlinked-parent collision, which is the known
cost in §5.
