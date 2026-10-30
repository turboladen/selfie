# 0014. A displayed text's kind is decided where the CLI receives it, and only the layout indents

Date: 2026-10-29

## Status

Accepted 2026-10-29.

## Context

`selfie-cli` prints text from outside selfie: spec, package and environment names, paths, spec
descriptions and commands, library messages, and what a tool printed. Any of it can carry characters
that act on the terminal instead of showing, or that make one name read as another.
`crates/cli/src/terminal_text.rs` escapes them, and every route to the terminal escapes. Three
decisions about that text are made in the wrong place.

**A text's kind is chosen call site by call site.** Each site picks an escaping function for the
value in front of it, and sites pick differently for the same value: one view escapes a dependency
name or a command as prose, so a zero-width space after `git` shows as nothing, while another
escapes the same kind of value strictly. A library sentence that interpolates a path is escaped as
prose, so an invisible character in a symlink's destination renders as nothing. The strict escape
also misses blanks that render as a space or as nothing without being controls or default-ignorable,
such as U+00A0 and U+2800, so `~/.bashrc` and `~/.bashrc` followed by U+2800 read the same.

**Continuation lines are indented in more than one layer.** An escaping function indents the lines
after the first, a printer indents them again, and some callers add their own, so a block comes out
at two different columns. An escape that skips the indent exists only to avoid the collision.

**Some guarantees rest on conventions no type reaches.**

- dialoguer prompt text and pick-list items are escaped by hand at each prompt.
- `shown!` rejects a format only when `{` is followed by an ASCII letter or `_`, so `{é}` and width
  or precision captures such as `{:>w$}` pass a value that was never checked as `Trusted`.
- `impl Trusted for &'static str` admits `Box::leak` and `String::leak`.
- Cleaning a tool's output is a step each caller must remember. A printer handed raw output escapes
  it as prose, which shows its color sequences as escapes instead of removing them.
- The test that keeps paths to `DisplayPath` finds test modules by counting braces, and braces
  inside string literals defeat it.
- Colors are switched off through `console`'s process-wide state, and the message printers style
  through that state, so no test can observe an explicit color decision.

Options considered:

- **Keep the escaping functions and review call sites harder.** That is the arrangement that keeps
  producing findings; review cannot see what a type does not say.
- **Let the library return typed text.** The library also serves the MCP server, whose JSON wants
  the raw string, and presentation stays out of the library by design. Rejected.
- **Decide the kind in the CLI, once, where it receives each value.** Chosen.

## Decision

### 1. A kind is a type

| Kind          | What it holds                                               | What it escapes                            |
| ------------- | ----------------------------------------------------------- | ------------------------------------------ |
| `Name`        | a spec, package, environment or file-stem name; a list item | the strict set, line breaks included       |
| `DisplayPath` | a path or a dotfile source                                  | the strict set                             |
| `Prose`       | a library sentence, description, note or refusal reason     | the guard set and a tab; line breaks kept  |
| `Output`      | what a tool printed                                         | cleaned as rule 5 says, then the guard set |
| `Content`     | one line of file content, as in a diff                      | the base set, a tab kept                   |
| `Command`     | a command for the user to run, or one a spec runs           | §5                                         |
| `Literal`     | selfie's own text                                           | nothing                                    |

1. Every text from outside selfie reaches the terminal as one of these kinds. A printer, a table
   cell and a prompt accept a kind or a `Shown` for any text they show, never a `String`, a `&str`
   or an `impl Display`. A value a prompt hands back rather than shows, such as a `Choice`'s raw
   value or a suggestion's original bytes (§6), stays raw: it is never written to the terminal as it
   is.
2. A kind holds its escaped text and is built only by its constructor. Its constructor is the only
   escaping call for that kind.
3. Escaping is one private function taking a `Policy` enum, with one variant per distinct escape:
   `Strict`, which `Name` and `DisplayPath` share; `Content`; `Prose`; `Output`; `CommandLine`, for
   a command selfie composes (§5.3); and `Guard`, which the printer's guard and a command a spec's
   author wrote (§5.4) share, since they escape the same set. The kind constructors and the guard
   are its only callers.
4. Three sets, each decided by one function and pinned by one test table:
   - the **guard set**, what can act on the terminal or reorder text: general category Cc other than
     a newline and a tab, Zl, Zp, the bidi controls (U+061C, U+200E, U+200F, U+202A to U+202E,
     U+2066 to U+2069) and the tag characters (U+E0000 to U+E007F). Joiners, variation selectors and
     emoji sequences render;
   - the **base set**: Cc, Cf, Zl, Zp and every default-ignorable code point;
   - the **strict set**: the base set, every Zs other than U+0020, and U+2800. The blanks are added
     only for names and paths, where two values that read alike are two different things. In file
     content U+00A0 and U+3000 are ordinary French and CJK text, so `Content` takes the base set.
5. `Output` is cleaned line by line before it is escaped: every ANSI escape sequence is removed;
   only the text after a line's last carriage return is kept, unless that text is blank, in which
   case the last non-blank part is kept, so a progress meter ending in a bare carriage return shows
   its final state; a tab becomes a space; and every other control character is dropped. The whole
   output is trimmed of blank space around it. No control character survives cleaning, so `Output`
   then needs only the guard set.
6. A `Shown` is a line assembled by `shown!` from values that are `Trusted`. `Trusted` is
   implemented for the kinds, `Literal` included, `Shown`, `SafeTable`, the integer and float types,
   and a reference to any of them, which `shown!` needs because it checks each argument by
   reference. It is implemented for nothing else.
7. A list of names is shown by joining `Name` values with a `Literal` separator. No function escapes
   and joins in one step.

### 2. The kind is decided where the CLI receives a value

1. The library keeps plain strings. One module in `crates/cli` maps each library field the CLI
   displays to its kind: `PackageInfoData::description` to `Prose`, `EnvironmentStatusData`'s
   dependency names to `Name`, its `install_command` and `CheckResultData::check_command` to
   `Command`. Command handlers read the mapped values and never format the raw field.
2. A library value's `Display` is `Prose`. Where that sentence interpolates a path or a name the
   user will act on, the CLI composes the sentence itself from the value's fields, with the path as
   a `DisplayPath`. `SkipReason::SecretDryRun`'s `LinkAtTarget::To` destination is composed this
   way.
3. A name inside a sentence the CLI composes is a `Name`, whatever the sentence around it is.

### 3. Only the layout indents

1. No kind and no `shown!` indents. `Prose` and `Output` keep their line breaks and their own
   leading whitespace, and add nothing.
2. The layer that places text on the screen hangs its continuation lines at the column where the
   text starts: a message printer under the text after its mark, a table inside the cell (§4), an
   entry row under the source column. Each text is indented by exactly one layer.
3. A block the library laid out, such as a secret conflict's summary, keeps its own indentation, and
   the layout adds only its column. Printed at column 0, it is unchanged.
4. A block whose lines must stay together, such as a diff, is rendered whole and then written in one
   draw, so no other line lands between two of its rows and a spinner does not redraw inside it.

### 4. One table style

1. Every table is a `SafeTable`: comfy-table's `UTF8_FULL_CONDENSED` with rounded corners, fitted to
   the terminal, with no separators between rows. Only `crates/cli/src/tables.rs` builds a
   comfy-table `Table`.
2. A cell takes a kind. A multi-line cell has its continuation lines indented two columns inside the
   cell by `SafeTable`, which is the layout layer for a table. With no row separators, a continued
   line is told from the next row by that indent and by the empty cells beside it.
3. `apply` and `dotfiles drift` keep their streaming aligned columns
   (`crates/cli/src/entry_rows.rs`) and do not become `SafeTable`s. Each of their rows prints as its
   entry is processed, in columns fixed from the planned entries before the first row; a `SafeTable`
   renders only once every row is known, which would hold the whole run's output until it ends. They
   take the same kinds, and a detail's continuation lines hang under the source column.

### 5. A displayed command stays runnable

Being executable matters most and readability second, so a command is never escaped as a `Name`: a
`Name`'s escape writes an invisible character as text such as `\u{200b}`, which changes the command
when it is pasted.

1. A command selfie composes, such as a remedy or a suggestion naming `selfie spec edit`, is built
   from `Literal` words, with each argument from outside selfie quoted as one shell word.
   `selfie::fs` exports one helper that quotes a word with `shlex::try_quote`, and
   `selfie::fs::shell_quote` calls it for a path after holding a leading `~/` outside the quotes, so
   the shell still expands it. A name argument goes to the word helper, not to `shell_quote`, so a
   name starting with `~` is quoted whole. `selfie-cli` quotes only through these two and does not
   depend on `shlex` itself.
2. `try_quote` (shlex 2.0.1) refuses only a string holding NUL. It leaves a word bare only when
   every byte is ASCII and in its safe set; any byte at or above 0x80 makes it quote, so `café`
   comes back as `'café'`. Inside the quotes it emits every byte other than the quote characters as
   it is, including a newline and the control bytes U+0001 to U+001F and U+007F, because no portable
   quoting exists for controls. Quoting therefore does not make an argument safe to print.
3. A composed command is one line. After quoting, it escapes the guard set, a newline and a tab:
   every Cc character, Zl, Zp, the bidi controls and the tag characters. An argument holding one of
   those, a newline included, is shown escaped and is not runnable as shown; no form of it is both
   safe to print and pasteable. An argument `try_quote` refuses is shown escaped as a `Name` and
   unquoted.
4. A command a spec's author wrote, such as an install or check command, is already shell text and
   is not quoted, since quoting would make it one word. It escapes the guard set only, so its line
   breaks and tabs are kept, and §3 lays out its lines.
5. A command holding a character the strict set escapes and rules 3 and 4 do not, such as U+200B,
   reads as another command while running correctly. A note under it shows the whole command with
   the strict set escaped, except that a spec's multi-line command keeps its line breaks, so each
   line of the note sits under the line it explains; §3 lays the note out. The command above the
   note stays as rules 3 and 4 leave it, so it still pastes and runs.

### 6. Only DisplayManager builds a prompt

1. `DisplayManager` owns the prompt methods, and nothing else constructs a dialoguer widget. Each
   method builds its widget and asks it through `DisplayManager::prompt`, which stays the one door
   to the terminal, with its `Prompt` trait private to `display_manager.rs`. So each method refuses
   when there is no terminal, ends the run on Ctrl+C, and keeps a spinner from drawing over the
   question; under test it takes its answer from the display's script, as every prompt does; and
   each returns `Result<_, PromptFailure>`:
   - `confirm(text: Shown) -> Result<bool, PromptFailure>`;
   - `select(text: Shown, choices: &[Choice<T>]) -> Result<T, PromptFailure>`, and a multi-select of
     the same shape, where a `Choice` pairs the `Name` it shows with the raw value it returns;
   - `input(text: Shown, suggested: Suggestion) -> Result<String, PromptFailure>`.
2. `input` decides how a suggestion is offered. When escaping it as a `Name` leaves it unchanged, it
   is pre-filled. Otherwise the prompt starts empty and a note above it shows the suggestion
   escaped. What an empty answer then means depends on whether the suggestion's bytes could be a
   valid answer. A `Suggestion` carries the suggested text and says which, and the caller builds it:
   - When they could, an empty answer returns them. `sync push`'s commit message is this case, and
     so is `spec create`'s environment name: an environment name is any YAML key, and the suggestion
     is the configured environment, which the user chose.
   - When they could not, the prompt offers no default, and an empty answer means what an empty
     answer means at that prompt. `track`'s spec name is this case: the spec-name rule refuses every
     character the strict set escapes, so a suggestion escaping would change can never be accepted,
     and an empty answer declines.

   No prompt pre-fills escaped text, and none hides a suggestion: the note always shows it.
3. Prompts draw on stderr, so their theme follows stderr's color decision (§8): `ColorfulTheme` when
   stderr gets colors, `SimpleTheme` when it does not. The methods choose it, and callers do not.
4. `crates/cli/clippy.toml` bans the constructors (`new` and `with_theme`) of `Confirm`, `Select`,
   `FuzzySelect`, `MultiSelect`, `Input`, `Sort` and `Password`, beside its existing bans on their
   `interact` methods. The prompt module in `display_manager.rs` holds the one `#[expect]`.

### 7. The remaining guarantees are checked, not trusted

1. `shown!` accepts a format whose only placeholders are `{}`, with `{{` and `}}` as literal braces.
   Any other character after an unescaped `{` fails to compile. This rejects named, inline, width
   and precision arguments alike.
2. `Literal` is built by `lit!`, which takes a string-literal token, so a leaked `String` cannot
   become one. Its constructor can run in a `const`, so a constant such as a diff separator is a
   `Literal`.
3. `Shown::__assembled` and `Literal`'s constructor are hidden, and a test rejects a direct call to
   either outside `terminal_text.rs`.
4. A path's text is taken for display only in `DisplayPath`. The test enforcing it parses each
   source file with `syn` to find the items under `#[cfg(test)]` and skip them, then walks every
   other item's tokens, macro arguments included, for a `.display()` call outside `DisplayPath`.
   String literals are single tokens and comments are not tokens, so neither can hide or fake a
   call. A clippy ban does not fit: it fires on the paths tests format into fixtures, and an
   `#[expect]` over a test module silences the CLI's other method bans along with it.
5. `to_string_lossy` is not gated. It also converts file names for completion and for name
   suggestions, which no syntax tells apart from display, and its result is a `String`, which no
   printer accepts (§1.1).

### 8. Colors are decided per stream

`DisplayManager` decides once, at construction, whether stdout and whether stderr get colors, from
the configured setting and what `console` decides for that stream, and styles every text it writes
with that stream's answer forced. It never sets `console`'s process-wide color state, so a test can
build a display with colors forced on and see them. The spinner and the prompts, which draw on
stderr and are styled by indicatif and dialoguer, take stderr's answer through what they are given:
a styled spinner template and `ColorfulTheme` when stderr gets colors, a plain template and
`SimpleTheme` when it does not.

## Consequences

- Each kind has one constructor and one policy, so the CLI has one way to show a name, a path, a
  sentence, a tool's output, a line of content and a command. An escaping helper on the side has no
  printer to hand its `String` to.
- A test reading back what a printer wrote strips selfie's styling from the one `Shown` the printer
  was given; no printer builds a second, plain copy for it.
- The spec-name rule refuses the four Hangul fillers (U+115F, U+1160, U+3164 and U+FFA0). They are
  the only strict-set characters `char::is_alphanumeric` admits, and refusing them is what makes
  §6.2's case for `track` hold.

Costs:

- Nearly every print call site changes. `print_warning(format!(..))` becomes a `shown!` over kinds,
  which is longer to write.
- A command holding an invisible character takes two lines, the command and its escaped note.
- A path converted by `to_string_lossy` and written into a `Prose` sentence is escaped at prose
  strictness. §2.2 keeps paths out of sentences where it matters, and no gate enforces it.
- Homoglyphs, such as a Cyrillic `а` in place of a Latin `a`, are not escaped by any kind.
- `syn` becomes a direct dev-dependency of `selfie-cli`.

Out of scope: the MCP server, which writes JSON and no terminal; the library's `Display`
implementations, which stay for logs and the MCP server; and non-UTF-8 paths, which `DisplayPath`
shows lossily, so two of them can read alike.

Reopen when selfie gains a second terminal renderer, such as a TUI, or when the library starts
returning messages as structured parts rather than sentences.
