pub mod common;

// A packages repository arrives through `sync pull` from commits made elsewhere,
// so its file names, spec text and file content can carry characters that act
// on the terminal: an escape sequence that clears the screen or retitles the
// window, a bidi override, an invisible character. These tests build such a
// repository and run every command that reads it, asserting that nothing of
// the kind reaches either stream except as a visible escape.

use std::fs;

use icu_properties::CodePointMapData;
use icu_properties::props::GeneralCategory;

use common::{SELFIE_ENV, git, sandboxed_std_command, setup_default_test_config};

// What each fixture field carries, written as YAML double-quoted escapes: CSI
// (clear screen), OSC (retitle, ended by BEL), a right-to-left override, a
// zero-width space, a tag character, a variation selector, then SGR sequences:
// two selfie's own styling also writes (red, dim), two it never does (black,
// bright black) and conceal. Each SGR follows a `q`, and the text ends in `z`,
// so a raw ESC after a `q` can only have come from the repository.
const HOSTILE: &str =
    r"\e[2J\e]0;pwn\a\u202E\u200B\U000E0041\uFE0Fq\e[31mq\e[2mq\e[30mq\e[90mq\e[8mz";

// The torture repository, in a fresh sandbox with a git repository at the
// package directory, a dotfiles directory and a state directory.
fn torture() -> tempfile::TempDir {
    let temp = setup_default_test_config();
    let home = temp.path();
    let packages = home.join("packages");
    let dotfiles = home.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::create_dir_all(home.join("state")).unwrap();
    let config = home.join(".config/selfie/config.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "dotfiles_directory: {}\nstate_directory: {}\n",
        dotfiles.display(),
        home.join("state").display()
    ));
    fs::write(&config, text).unwrap();

    // A source whose name carries the escape sequence, one whose name tries to
    // start a line of its own, and content with an escape and a tab.
    fs::write(
        packages.join("src\u{1b}[2Jfile"),
        "line\u{1b}[2J\tcontent\n",
    )
    .unwrap();
    fs::write(packages.join("x\n✓ fake"), "fake\n").unwrap();
    fs::write(
        packages.join("torture.yml"),
        format!(
            "name: torture\ndescription: \"desc{HOSTILE} \u{1F468}\u{200D}\u{1F4BB}\"\nhomepage: \"https://x{HOSTILE}\"\n\
             post_install_note: \"first{HOSTILE}\\nsecond{HOSTILE}\"\n\
             dotfiles:\n  - source: \"src\\e[2Jfile\"\n    target: \"~/.t{HOSTILE}\"\n\
             \x20 - source: \"x\\n✓ fake\"\n    target: \"~/.fake\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"touch installed\"\n\
             \x20   check: \"test -e installed && echo 'k{HOSTILE}' # c{HOSTILE}\"\n\
             \x20   audit: \"echo 'a{HOSTILE}'\"\n"
        ),
    )
    .unwrap();
    // A spec file whose name holds a line break, so a listing's name cell
    // would draw a row of its own if it kept the break.
    fs::write(packages.join("y\n✓ fake.yml"), "name: y\n").unwrap();
    // A spec with a key selfie does not know, which it refuses, naming the key.
    fs::write(
        packages.join("unknown.yml"),
        format!("name: unknown\n\"key{HOSTILE}\": 1\n"),
    )
    .unwrap();
    // Two installs that fail with an escape sequence and an override on stderr,
    // one the other's dependency, so the run ends with an error summary.
    for (name, depends) in [("failone", "[failtwo]"), ("failtwo", "[]")] {
        fs::write(
            packages.join(format!("{name}.yml")),
            format!(
                "name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo 'e{HOSTILE}\\n✓ fake line' >&2; \
                 false\"\n    dependencies: {depends}\n"
            ),
        )
        .unwrap();
    }
    // A spec file whose own name carries the escape sequence: not a valid spec
    // name, so every listing names it as a file it could not use.
    fs::write(
        packages.join("bad\u{1b}[2Jname.yml"),
        format!("name: bad\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();
    fs::write(dotfiles.join("loose.conf"), "loose\n").unwrap();
    fs::write(
        dotfiles.join("loose.yml"),
        format!("name: loose\ndotfiles:\n  - source: loose.conf\n    target: \"~/.lo{HOSTILE}\"\n"),
    )
    .unwrap();
    // A file to track, whose name carries the escape sequence.
    fs::write(home.join("track\u{1b}[2Jme"), "tracked\n").unwrap();
    git(&packages, &["init", "--quiet"]);
    temp
}

// Whether `c` must never reach a stream raw: what acts on the terminal, breaks
// a line or reorders text. Joiners, variation selectors and other invisible
// characters may render in prose; names and paths escape them too, which the
// expected strings below check.
fn hostile(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    ) || ('\u{E0000}'..='\u{E007F}').contains(&c)
}

// Whether `line` starts with the fake text, or holds a table cell that does.
fn starts_with_fake_text(line: &str) -> bool {
    if line.starts_with('│') {
        // A cell is padded with one space; a line a cell's text continues on is
        // indented further.
        line.split(['│', '┆'])
            .any(|cell| cell.strip_prefix(' ').unwrap_or(cell).starts_with("✓ fake"))
    } else {
        line.starts_with("✓ fake")
    }
}

// `output` with selfie's own color and weight sequences removed.
fn without_styling(output: &str) -> String {
    let mut shown = String::new();
    let mut rest = output;
    while let Some(c) = rest.chars().next() {
        if let Some(body) = rest.strip_prefix("\u{1b}[")
            && let Some(end) = body.find('m')
            && own_sgr(&body[..end])
        {
            rest = &body[end + 1..];
            continue;
        }
        shown.push(c);
        rest = &rest[c.len_utf8()..];
    }
    shown
}

// An SGR sequence's parameters as selfie's own styling writes them: one per
// sequence, from the styles it uses.
fn own_sgr(sequence: &str) -> bool {
    sequence
        .parse::<u16>()
        .is_ok_and(|p| matches!(p, 0..=4 | 31..=36))
}

// What a run wrote to stdout and stderr, together, that should not be there:
// a hostile character outside a sequence selfie's own styling emits, and with
// `colored` false any escape sequence at all. A tab is allowed where `tabs`.
fn raw_leaks(output: &str, colored: bool, tabs: bool) -> Vec<String> {
    let mut leaks = Vec::new();
    let mut rest = output;
    while let Some(c) = rest.chars().next() {
        if colored
            && c == '\u{1b}'
            && let Some(body) = rest.strip_prefix("\u{1b}[")
            && let Some(end) = body.find('m')
            && body[..end].chars().all(|c| c.is_ascii_digit() || c == ';')
            && own_sgr(&body[..end])
        {
            rest = &body[end + 1..];
            continue;
        }
        let allowed = c == '\n' || (tabs && c == '\t');
        if !allowed && hostile(c) {
            let at = output.len() - rest.len();
            let from = output[..at]
                .char_indices()
                .rev()
                .nth(20)
                .map_or(0, |(i, _)| i);
            leaks.push(format!("U+{:04X} after {:?}", c as u32, &output[from..at]));
        }
        rest = &rest[c.len_utf8()..];
    }
    leaks
}

struct Run {
    code: Option<i32>,
    both: String,
}

fn run(temp: &tempfile::TempDir, args: &[&str], colored: bool) -> Run {
    let mut command = sandboxed_std_command(temp);
    // Colors forced on whatever the stream is, so `--no-color` alone has to
    // keep every escape sequence off it.
    command.env("CLICOLOR_FORCE", "1").env_remove("NO_COLOR");
    // A verbose run's log lines are tracing's, colored by tracing, which reads
    // NO_COLOR. console ignores NO_COLOR under CLICOLOR_FORCE, so selfie's own
    // output keeps the color state the pass asks for.
    if args.first() == Some(&"--verbose") {
        command.env("NO_COLOR", "1");
    }
    if !colored {
        command.arg("--no-color");
    }
    let output = command.args(args).output().unwrap();
    Run {
        code: output.status.code(),
        both: without_log_lines(&format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

// `output` without a verbose run's log lines, which tracing writes and escapes
// by its own rules; this file checks what selfie's printers write. Each log line
// starts with its timestamp.
fn without_log_lines(output: &str) -> String {
    output
        .split_inclusive('\n')
        .filter(|line| !(line.starts_with("20") && line.get(10..11) == Some("T")))
        .collect()
}

// Each command, what it must show escaped to prove it reached the fixture, and
// whether its output may hold a tab (a diff of content that has one).
const COMMANDS: &[(&[&str], &str, bool)] = &[
    (&["apply", "--dry-run"], r"src\u{1b}[2Jfile", false),
    (
        &["apply", "torture", "--dry-run"],
        r"src\u{1b}[2Jfile",
        false,
    ),
    // A path is a name, so even what renders as nothing in prose is escaped.
    (
        &["dotfiles", "list"],
        r".t\u{1b}[2J\u{1b}]0;pwn\u{7}\u{202e}\u{200b}",
        false,
    ),
    // A description is prose, so an emoji sequence renders.
    (
        &["spec", "info", "torture"],
        "\u{1F468}\u{200D}\u{1F4BB}",
        false,
    ),
    (&["spec", "list"], r"desc\u{1b}[2J", false),
    (&["spec", "info", "torture"], r"desc\u{1b}[2J", false),
    (&["spec", "search", "tort"], r"desc\u{1b}[2J", false),
    (&["spec", "validate", "torture"], "homepage", false),
    (&["spec", "validate", "unknown"], r"key\u{1b}[2J", false),
    (&["spec", "validate", "--all"], r"bad\u{1b}[2Jname", false),
    (&["package", "list"], r"bad\u{1b}[2Jname", false),
    (&["package", "check", "torture"], "torture", false),
    (&["package", "audit", "torture"], r"a\u{1b}[2J", false),
    (&["package", "status", "torture"], r"c\u{1b}[2J", false),
    (
        &["package", "install", "torture"],
        // The note is one message: its second line is indented under the first.
        "\n  second\\u{1b}[2J",
        false,
    ),
    // Under --verbose a check prints a card with the check's own output.
    (
        &["--verbose", "package", "check", "torture"],
        r": k\u{202e}",
        false,
    ),
    (&["package", "install", "failone"], r"e\u{1b}[2J", false),
    // Under --verbose a command's output is printed as it arrives, cleaned of
    // its colors, with its format characters shown as escapes.
    (
        &["--verbose", "package", "install", "failone"],
        r"failtwo │ e\u{202e}",
        false,
    ),
    (&["sync", "status"], r"bad\u{1b}[2Jname", false),
    // A name cell keeps a name holding a line break on one line.
    (&["package", "list"], r"y\n✓ fake", false),
    (&["dotfiles", "drift"], r".t\u{1b}[2J", false),
    // A path in a message is escaped as a name, though the message around it
    // is prose.
    (
        &["apply", "--dry-run"],
        r".lo\u{1b}[2J\u{1b}]0;pwn\u{7}\u{202e}\u{200b}",
        true,
    ),
    (&["apply"], r"bad\u{1b}[2Jname", false),
    (
        &["dotfiles", "track", "trk", "~/track\u{1b}[2Jme"],
        r"track\u{1b}[2Jme",
        false,
    ),
    (
        &["spec", "remove", "torture", "--yes"],
        r".t\u{1b}[2J",
        false,
    ),
];

// Which of a colored run's two guards a check watches. Text from outside selfie
// is escaped where it is built (provenance): a name strictly, so it shows its
// escapes and stays on one line, and nothing the repository styled survives.
// The printer then writes no sequence but those selfie's styles write (codes).
// Checking them apart shows each one holds when the other does not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    Both,
    Codes,
    Provenance,
}

fn check_every_command(colored: bool, guard: Guard) {
    let temp = torture();
    let mut failures = Vec::new();
    // Controls: a run with colors shows selfie's own styling somewhere, and the
    // name holding a newline shows somewhere, or the checks below see nothing.
    let mut styled = 0;
    let mut fake_names = 0;
    let mut fake_lines = 0;
    for (args, escaped, tabs) in COMMANDS {
        let run = run(&temp, args, colored);
        styled += usize::from(run.both.contains("\u{1b}["));
        fake_names += run.both.matches("x\\n✓ fake").count();
        fake_lines += run.both.matches("✓ fake line").count();
        if guard != Guard::Codes {
            // Selfie's own styling cannot be told from a repository's by its
            // parameters, only by where it came from.
            if run.both.contains("q\u{1b}[") {
                failures.push(format!(
                    "{args:?} kept a repository's styling:\n{}",
                    run.both
                ));
            }
        }
        if guard != Guard::Provenance {
            let leaks = raw_leaks(&run.both, colored, *tabs);
            if !leaks.is_empty() {
                failures.push(format!("{args:?} ({:?}) leaked: {leaks:#?}", run.code));
            }
        }
        if guard == Guard::Codes {
            continue;
        }
        if !run.both.contains(escaped) {
            failures.push(format!(
                "{args:?} ({:?}) does not show {escaped}:\n{}",
                run.code, run.both
            ));
        }
        // A post-install note keeps its line breaks: its second line starts a
        // line, and never shows as an escaped newline after the first.
        if run.both.contains("\\nsecond") {
            failures.push(format!(
                "{args:?} ran the note's lines together:\n{}",
                run.both
            ));
        }
        // Text from the repository never starts a line at the left margin or a
        // table's first column, where it could pass for selfie's own result line
        // or a row of its own: a name keeps its newline escaped, and a message or
        // command output indents the lines after its first.
        if let Some(line) = without_styling(&run.both)
            .lines()
            .find(|line| starts_with_fake_text(line))
        {
            failures.push(format!(
                "{args:?} starts a line with the fake text, {line:?}:\n{}",
                run.both
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert_eq!(styled > 0, colored, "{styled} commands printed styling");
    assert!(
        fake_names > 0,
        "no command showed the name holding a newline"
    );
    assert!(
        fake_lines > 0,
        "no command showed output holding a line of its own"
    );
}

#[test]
fn no_command_lets_a_hostile_character_through_without_color() {
    check_every_command(false, Guard::Both);
}

// Whatever reaches a printer, a colored run writes no sequence but those
// selfie's styles write.
#[test]
fn a_colored_run_writes_only_selfies_own_sequences() {
    check_every_command(true, Guard::Codes);
}

// No sequence from the repository survives into a colored run, not even one
// selfie's styles also write.
#[test]
fn a_colored_run_keeps_no_styling_from_the_repository() {
    check_every_command(true, Guard::Provenance);
}

// A refusal names its source and target in a library sentence, and a real apply
// prints it on stderr: the sequence shows escaped, and no ESC reaches stderr.
#[test]
fn a_refusal_naming_a_hostile_path_shows_it_escaped() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::create_dir_all(packages.join("esc")).unwrap();
    fs::write(packages.join("esc").join("a\u{1b}[2Jb"), "e\n").unwrap();
    fs::write(
        packages.join("esc.yaml"),
        format!(
            "name: esc\ndotfiles:\n  - source: \"esc/a\\e[2Jb\"\n    target: \"~/.esc\\e[2J\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    // A directory at the target, which apply refuses to replace.
    fs::create_dir_all(temp.path().join(".esc\u{1b}[2J")).unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(
        stderr.contains(r"esc/a\u{1b}[2Jb") && stderr.contains(r".esc\u{1b}[2J"),
        "{stderr}"
    );
}

// A configuration that cannot load is reported through the display, so a path
// in the message shows escaped, and the run still fails.
#[test]
fn a_startup_error_naming_a_hostile_path_shows_it_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("conf\u{1b}[2Jdir");
    fs::create_dir_all(&config_dir).unwrap();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("selfie"))
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join(".config"))
        .env("SELFIE_CONFIG_DIR", &config_dir)
        .env("SHELL", "/bin/sh")
        .args(["--no-color", "package", "list"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(common::FAILED), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains(r"conf\u{1b}[2Jdir"), "{stderr}");
    assert!(stderr.starts_with("✗ "), "{stderr}");
}

// A parse error's source window shows the failing line escaped, and the caret
// stays under the character the parser named, past the escapes before it.
#[test]
fn a_parse_error_caret_lands_past_escaped_characters() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("esc.yml"),
        "name: esc\ndescription: a\u{200B}\u{200B}b: c\n",
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "validate", "esc"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    let line = stderr
        .lines()
        .find(|line| line.contains("description:"))
        .unwrap_or_else(|| panic!("no failing line in:\n{stderr}"));
    assert!(line.ends_with(r"a\u{200b}\u{200b}b: c"), "{stderr}");
    let caret = stderr
        .lines()
        .find(|line| line.trim_end().ends_with('^'))
        .unwrap_or_else(|| panic!("no caret in:\n{stderr}"));
    // Byte offsets: everything before the colon on both lines is ASCII.
    assert_eq!(caret.find('^'), line.find(": c"), "{stderr}");
}
