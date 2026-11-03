pub mod common;

// A packages repository arrives through `sync pull` from commits made elsewhere,
// so its file names, spec text and file content can carry characters that act
// on the terminal: an escape sequence that clears the screen or retitles the
// window, a bidi override, an invisible character. These tests build such a
// repository and run every command that reads it, asserting that nothing of
// the kind reaches either stream except as a visible escape.

use std::fs;

use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};

use common::{
    FAILED, SELFIE_ENV, git, package_repo_with_remote, sandboxed_std_command,
    setup_default_test_config,
};

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
    // A target that already holds other content, so apply shows a conflict
    // diff of the source's content, tab and escape sequence included.
    fs::write(
        home.join(
            ".t\u{1b}[2J\u{1b}]0;pwn\u{7}\u{202E}\u{200B}\u{E0041}\u{FE0F}\
             q\u{1b}[31mq\u{1b}[2mq\u{1b}[30mq\u{1b}[90mq\u{1b}[8mz",
        ),
        "other\n",
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

// Whether `c` is in the strict set but not `hostile`: a format or
// default-ignorable character, a blank other than a space, or U+2800.
fn strict_only(c: char) -> bool {
    let category = CodePointMapData::<GeneralCategory>::new().get(c);
    category == GeneralCategory::Format
        || (category == GeneralCategory::SpaceSeparator && c != ' ')
        || c == '\u{2800}'
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(c)
}

// What the verbose log holds raw that the strict set escapes, other than each
// record's own line break.
fn strict_leaks(log: &str) -> Vec<String> {
    log.chars()
        .filter(|&c| c != '\n' && (hostile(c) || strict_only(c)))
        .map(|c| format!("U+{:04X}", c as u32))
        .collect()
}

struct Run {
    code: Option<i32>,
    // This holds everything the run wrote, stdout then stderr.
    both: String,
    // This holds the same without the lines that start a verbose log event,
    // which leaves what selfie's printers wrote and the continuation lines of a
    // multi-line event.
    printed: String,
    // This holds the lines that start a verbose log event.
    log: String,
}

fn run(temp: &tempfile::TempDir, args: &[&str], colored: bool) -> Run {
    let mut command = sandboxed_std_command(temp);
    // Colors forced on whatever the stream is, so `--no-color` alone has to
    // keep every escape sequence off it.
    command.env("CLICOLOR_FORCE", "1").env_remove("NO_COLOR");
    if !colored {
        command.arg("--no-color");
    }
    let output = command.args(args).output().unwrap();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (log, printed): (Vec<&str>, Vec<&str>) = both
        .split_inclusive('\n')
        .partition(|line| is_log_line(line));
    Run {
        code: output.status.code(),
        printed: printed.concat(),
        log: log.concat(),
        both,
    }
}

// Whether `line` is the first line of a verbose log event, which starts with
// the event's timestamp. The log is never colored, so nothing comes before it.
fn is_log_line(line: &str) -> bool {
    line.starts_with("20") && line.get(10..11) == Some("T")
}

// Each command, what it must show escaped to prove it reached the fixture, and
// whether its output may hold a tab (a diff of content that has one).
const COMMANDS: &[(&[&str], &str, bool)] = &[
    (&["apply", "--dry-run"], r"+line\u{1b}[2J", true),
    (&["apply", "torture", "--dry-run"], r"+line\u{1b}[2J", true),
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
    (&["apply"], r"+line\u{1b}[2J", true),
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
        styled += usize::from(run.printed.contains("\u{1b}["));
        fake_names += run.printed.matches("x\\n✓ fake").count();
        fake_lines += run.printed.matches("✓ fake line").count();
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
        // A log line could show the value escaped too, so only what the
        // printers wrote counts here.
        if !run.printed.contains(escaped) {
            failures.push(format!(
                "{args:?} ({:?}) does not show {escaped}:\n{}",
                run.code, run.printed
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

// A check command's text reaches the verbose log in a message, which tracing
// escapes only ESC in: its override and tag character arrive raw, and the log
// shows them as escapes.
fn check_the_log_escapes_a_spec_value(colored: bool) {
    let temp = torture();
    let run = run(
        &temp,
        &["--verbose", "package", "check", "torture"],
        colored,
    );
    // Controls: the run logged, and the hostile value reached the log.
    assert!(run.log.lines().count() > 0, "no log lines:\n{}", run.both);
    for escaped in [r"k\x1b[2J", r"\u{202e}", r"\u{e0041}"] {
        assert!(
            run.log
                .lines()
                .any(|line| line.contains("Found check command") && line.contains(escaped)),
            "no log line shows {escaped}:\n{}",
            run.log
        );
    }
    let leaks = raw_leaks(&run.log, false, false);
    assert!(leaks.is_empty(), "the log leaked: {leaks:#?}");
}

#[test]
fn the_verbose_log_shows_a_spec_value_escaped_without_color() {
    check_the_log_escapes_a_spec_value(false);
}

#[test]
fn the_verbose_log_shows_a_spec_value_escaped_with_color() {
    check_the_log_escapes_a_spec_value(true);
}

// A check command over several lines is logged as one message. Its line breaks
// show escaped on the record's own line, so no line of it can pass for a log
// line of its own or start at the left margin with selfie's result mark.
#[test]
fn a_logged_value_on_several_lines_stays_on_its_records_line() {
    // A line that passes for a log record of its own.
    let forged = "2026-10-31T00:00:00.000000Z  INFO forged";
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("lines.yml"),
        format!(
            "name: lines\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    check: \": '\\n✓ fake \
             line\\u202E\\U000E0041\\rz\\n{forged}\\n'\"\n"
        ),
    )
    .unwrap();
    let run = run(&temp, &["--verbose", "package", "check", "lines"], false);
    let starts: Vec<&str> = run
        .both
        .lines()
        .filter(|line| line.starts_with(forged))
        .collect();
    assert!(starts.is_empty(), "{starts:#?}");
    let fake: Vec<&str> = run
        .both
        .lines()
        .filter(|line| line.starts_with("✓ fake"))
        .collect();
    assert!(fake.is_empty(), "{fake:#?}");
    let leaks = strict_leaks(&run.log);
    assert!(leaks.is_empty(), "{leaks:#?}");
    let leaks = raw_leaks(&run.both, false, false);
    assert!(leaks.is_empty(), "{leaks:#?}");
    // Control: the whole command reached the log, each record holding it on
    // its own line, its line breaks, override, tag character and carriage
    // return escaped.
    let found: Vec<&str> = run
        .log
        .lines()
        .filter(|line| line.contains("Found check command"))
        .collect();
    assert!(!found.is_empty(), "{}", run.both);
    let whole = format!(r"'\n✓ fake line\u{{202e}}\u{{e0041}}\rz\n{forged}\n'");
    for line in found {
        assert!(line.contains(&whole), "{line}");
    }
}

// A refusal logs its sentence in a field named `message`, which tracing formats
// with Display. A target holding a zero-width space, an override and a line
// feed shows them escaped on the warning's own line.
fn check_a_refused_target_is_logged_escaped_on_one_line(command: &[&str]) {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::write(packages.join("zw"), "z\n").unwrap();
    fs::write(
        packages.join("zw.yaml"),
        format!(
            "name: zw\ndotfiles:\n  - source: zw\n    target: \"~/t\\u200B\\u202E\\nx\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    // A directory at the target, which apply refuses to replace.
    fs::create_dir_all(temp.path().join("t\u{200b}\u{202e}\nx")).unwrap();

    let run = run(&temp, &[&["--verbose"], command].concat(), false);
    assert_eq!(run.code, Some(FAILED), "{}", run.both);
    // The sentence quotes the target with `'`; the `spec_target` field beside
    // it is Debug, quoted with `"`, so only the sentence matches.
    assert!(
        run.log
            .lines()
            .any(|line| line.contains(" WARN ") && line.contains(r"t\u{200b}\u{202e}\nx'")),
        "{}",
        run.both
    );
    let leaks = strict_leaks(&run.log);
    assert!(leaks.is_empty(), "{leaks:#?}");
}

#[test]
fn apply_logs_a_refused_target_escaped_on_one_line() {
    check_a_refused_target_is_logged_escaped_on_one_line(&["apply"]);
}

#[test]
fn drift_logs_a_refused_target_escaped_on_one_line() {
    check_a_refused_target_is_logged_escaped_on_one_line(&["dotfiles", "drift"]);
}

// The log is plain on a colored stderr: it holds no escape sequence, raw or
// escaped, while the printers' own lines on the same stream are colored.
#[test]
fn a_colored_run_writes_a_plain_log() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("plain.yml"),
        format!("name: plain\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();
    let run = run(&temp, &["--verbose", "spec", "info", "plain"], true);
    assert_eq!(run.code, Some(0), "{}", run.both);
    assert!(run.log.lines().count() > 0, "no log lines:\n{}", run.both);
    assert!(!run.log.contains('\u{1b}'), "{:?}", run.log);
    // tracing styles nothing either, or its styling would show as escapes.
    assert!(!run.log.contains(r"\u{1b}"), "{}", run.log);
    // Control: the run had colors.
    assert!(run.printed.contains("\u{1b}["), "{:?}", run.printed);
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

// A refused entry names its source and target as paths, so on `apply` and
// `dotfiles drift` a U+200B in one shows escaped, and a line feed in one does
// not split the line.
#[test]
fn a_refusal_names_its_target_as_a_path() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::create_dir_all(packages.join("pk")).unwrap();
    fs::write(packages.join("pk").join("rc"), "e\n").unwrap();
    fs::write(
        packages.join("pk.yaml"),
        format!(
            "name: pk\ndotfiles:\n  - source: pk/rc\n    target: \"~/t\\u200Bx\"\n  \
             - source: pk/rc\n    target: \"~/l\\nf\"\nenvironments:\n  {SELFIE_ENV}:\n    \
             install: \"true\"\n"
        ),
    )
    .unwrap();
    // A directory at each target, which apply refuses to replace.
    fs::create_dir_all(temp.path().join("t\u{200B}x")).unwrap();
    fs::create_dir_all(temp.path().join("l\nf")).unwrap();

    for command in [&["apply"][..], &["dotfiles", "drift"]] {
        let output = sandboxed_std_command(&temp)
            .arg("--no-color")
            .args(command)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!stderr.contains('\u{200B}'), "{command:?}: {stderr:?}");
        assert!(
            stderr.lines().any(|line| line.contains(r"t\u{200b}x'")),
            "{command:?}: {stderr}"
        );
        assert!(
            stderr.lines().any(|line| line.contains(r"l\nf'")),
            "{command:?}: {stderr}"
        );
        assert!(
            !stderr.lines().any(|line| line.starts_with("f'")),
            "{command:?}: {stderr}"
        );
    }
}

// A failed write names the directory it could not write in as a path, so a
// U+200B in it shows escaped on the error line.
#[cfg(unix)]
#[test]
fn a_failed_write_names_its_path_as_a_path() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::create_dir_all(packages.join("pk")).unwrap();
    fs::write(packages.join("pk").join("rc"), "e\n").unwrap();
    fs::write(
        packages.join("pk.yaml"),
        format!(
            "name: pk\ndotfiles:\n  - source: pk/rc\n    target: \"~/r\\u200Bo/rc\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    let shut = temp.path().join("r\u{200B}o");
    fs::create_dir_all(&shut).unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores the mode, so there is no failure to name.
    if fs::write(shut.join("probe"), "x").is_ok() {
        return;
    }

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply"])
        .output()
        .unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr
            .lines()
            .any(|line| line.contains("Failed to write") && line.contains(r"r\u{200b}o/rc")),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A secret entry over a symlink replaces the link and warns, naming the link
// and its destination as paths, so a U+200B in either shows escaped.
#[cfg(unix)]
#[test]
fn a_replaced_link_warning_names_its_paths_as_paths() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::create_dir_all(&packages).unwrap();
    fs::write(
        packages.join("pk.yaml"),
        format!(
            "name: pk\ndotfiles:\n  - command: \"echo hi\"\n    target: \"~/s\\u200Bl\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    fs::write(temp.path().join("v\u{200B}ault"), "old").unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("v\u{200B}ault"),
        temp.path().join("s\u{200B}l"),
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.lines().any(
            |line| line.contains(r"s\u{200b}l', which was a symlink to '")
                && line.contains(r"v\u{200b}ault'")
        ),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A named package with nothing to apply in the environment names both as names,
// so a U+200B in the environment shows escaped.
#[test]
fn a_package_with_nothing_to_apply_names_the_environment_as_a_name() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::create_dir_all(&packages).unwrap();
    fs::write(
        packages.join("pk.yaml"),
        "name: pk\nenvironments:\n  \"t\\u200Best\":\n    install: \"true\"\n",
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "--environment", "t\u{200B}est", "apply", "pk"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr
            .lines()
            .any(|line| line
                .contains(r"Package 'pk' has no dotfiles for environment 't\u{200b}est'")),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A package with no configuration for the environment in use names the
// environment as a name, so a U+200B and a right-to-left override in it show
// escaped.
#[test]
fn package_status_names_an_unconfigured_environment_escaped() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("pk.yml"),
        format!("name: pk\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args([
            "--no-color",
            "--environment",
            "w\u{200B}o\u{202E}k",
            "package",
            "status",
            "pk",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.lines().any(|line| line
            .contains(r"Package 'pk' has no configuration for environment 'w\u{200b}o\u{202e}k'")),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A spec file a validation could not compare with is named by its label, such
// as `packages/<file>`, as a name, so a U+200B in its file name shows escaped.
#[test]
fn spec_validate_names_a_spec_it_could_not_check_escaped() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::write(
        packages.join("a.yml"),
        format!(
            "name: a\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    dotfiles:\n\
             \x20     - source: rc\n        target: ~/.rc\n"
        ),
    )
    .unwrap();
    fs::write(packages.join("x\u{200B}.yml"), "name: [unclosed\n").unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "validate", "a"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr
            .lines()
            .any(|line| line.contains(r"Could not check packages/x\u{200b}.yml for an entry")),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A package directory outside any git repository is named as a path, so a
// U+200B and a line feed in it show escaped on the error's one line.
#[test]
fn sync_push_names_a_directory_outside_a_repository_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let packages = temp.path().join("p\u{200B}k\nx");
    fs::create_dir_all(&packages).unwrap();
    let config = temp.path().join(".config").join("selfie");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: \"{}\"\n",
            packages.display().to_string().replace('\n', "\\n")
        ),
    )
    .unwrap();
    // Discovery walks up from the directory, and selfie's does not read
    // GIT_CEILING_DIRECTORIES, so a TMPDIR inside a work tree finds that one.
    let inside = common::isolated_git()
        .arg("-C")
        .arg(&packages)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "the sandbox {} is inside a git work tree, so this test cannot run here",
        temp.path().display()
    );

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "sync", "push", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(FAILED), "{stderr}");
    assert!(
        stderr.lines().any(
            |line| line.contains("not a git repository: ") && line.ends_with(r"/p\u{200b}k\nx")
        ),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A conflict's diff shows each line of content with its escape sequences as
// escapes and its tabs as tabs, and a content line starting `-- ` or `++ ` as
// content, not as a header.
#[test]
fn a_conflict_diff_shows_content_escaped_with_tabs() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::write(
        packages.join("conf"),
        "one\u{1b}[2J\u{200B}\tx\n-- removed looks like a header\n++ added looks like one too\n",
    )
    .unwrap();
    fs::write(
        packages.join("conf.yaml"),
        format!(
            "name: conf\ndotfiles:\n  - source: conf\n    target: \"~/.conf\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    fs::write(
        temp.path().join(".conf"),
        "other\n-- gone looks like a header too\n",
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    // Content is escaped as a name is, so a character that renders as nothing
    // still shows, while its tab stays a tab.
    assert!(stdout.contains("+one\\u{1b}[2J\\u{200b}\tx"), "{stdout}");
    // One rule under the file names and one closing the diff. A content line
    // taken for a `+++ ` header draws another.
    let rules = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("──"))
        .count();
    assert_eq!(rules, 2, "{stdout}");
    assert!(
        stdout.contains("  --- gone looks like a header too\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+-- removed looks like a header"),
        "{stdout}"
    );
    assert!(stdout.contains("+++ added looks like one too"), "{stdout}");
}

// A target whose name holds a line break keeps its diff header on one line,
// so the rest of the name cannot pass for a hunk.
#[test]
fn a_diff_header_naming_a_line_break_stays_on_its_line() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::write(packages.join("nl"), "new\n").unwrap();
    fs::write(
        packages.join("nl.yaml"),
        format!(
            "name: nl\ndotfiles:\n  - source: nl\n    target: \"~/.nl\\n@@ -9 +9 @@\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    fs::write(temp.path().join(".nl\n@@ -9 +9 @@"), "old\n").unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.lines().any(
            |line| line.trim_start().starts_with("--- ") && line.ends_with(r".nl\n@@ -9 +9 @@")
        ),
        "{stdout}"
    );
    assert!(
        !stdout.lines().any(|line| line.trim() == "@@ -9 +9 @@"),
        "{stdout}"
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

// A sentence `spec remove` composes names the spec as a name, so an invisible
// character in the name it was given shows as an escape.
#[test]
fn spec_remove_names_the_spec_escaped() {
    let temp = setup_default_test_config();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "remove", "zw\u{200B}x", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(r"Package 'zw\u{200b}x' not found."),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// `spec edit` names the spec it was given as a name when it cannot open an
// editor, so an invisible character in the name shows as an escape.
#[test]
fn spec_edit_without_an_editor_names_the_spec_escaped() {
    let temp = setup_default_test_config();

    let output = sandboxed_std_command(&temp)
        .env_remove("EDITOR")
        .args(["--no-color", "spec", "edit", "zw\u{200B}x"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(r"Package 'zw\u{200b}x' doesn't exist yet."),
        "{stderr}"
    );
}

// `spec edit` names the spec as a name when what answers to it cannot be used,
// whatever the library's sentence after it says.
#[test]
fn spec_edit_refusing_an_unusable_spec_names_it_escaped() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("zw\u{200B}q.yml"),
        "name: zwq\n",
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .env_remove("EDITOR")
        .args(["--no-color", "spec", "edit", "zw\u{200B}q"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(r"Cannot edit 'zw\u{200b}q': selfie could not use"),
        "{stderr}"
    );
}

// An install names a recommend that failed as a name.
#[test]
fn an_install_names_a_failed_recommend_escaped() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("recs.yml"),
        format!(
            "name: recs\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
             recommends: [\"re\\u200Bc\"]\n"
        ),
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "package", "install", "recs"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(stderr.contains(r"re\u{200b}c failed:"), "{stderr}");
}

// A configuration file that will not parse is named as a path, so a line
// break in its directory's name stays on the error's line and U+200B shows.
#[test]
fn a_configuration_file_that_will_not_parse_names_its_path_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("cfg\nzw\u{200B}dir");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.yaml"), "environment: [\n").unwrap();

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
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains(r"Cannot parse the configuration file ")
            && stderr.contains(r"cfg\nzw\u{200b}dir/config.yaml: "),
        "{stderr}"
    );
}

// `config validate` names a configuration file that will not parse as a path,
// as a command's startup error does.
#[test]
fn config_validate_names_a_configuration_file_that_will_not_parse_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("cfg\nzw\u{200B}dir");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.yaml"), "environment: [\n").unwrap();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("selfie"))
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join(".config"))
        .env("SELFIE_CONFIG_DIR", &config_dir)
        .env("SHELL", "/bin/sh")
        .args(["--no-color", "config", "validate"])
        .output()
        .unwrap();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(both.contains(r"cfg\nzw\u{200b}dir/config.yaml: "), "{both}");
    assert!(!both.contains('\u{200B}'), "{both:?}");
}

// A spec file whose name breaks the rule is named alike in the listing and in
// the sentence beside it.
#[test]
fn an_invalid_spec_name_reads_as_it_does_in_the_listing() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("zw\u{200B}q.yml"),
        "name: zwq\n",
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "list"])
        .output()
        .unwrap();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        both.contains(r"its name 'zw\u{200b}q' is not a valid spec name"),
        "{both}"
    );
    assert!(!both.contains('\u{200B}'), "{both:?}");
}

// `spec create` refuses a name that breaks the rule naming it as a name.
#[test]
fn spec_create_refuses_an_invalid_name_escaped() {
    let temp = setup_default_test_config();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "create", "zw\u{200B}x"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(r"Refusing to create 'zw\u{200b}x': it is not a valid spec name"),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// `spec edit` of a missing spec whose name breaks the rule refuses it as
// `spec create` does.
#[test]
fn spec_edit_refuses_an_invalid_name_escaped() {
    let temp = setup_default_test_config();

    let output = sandboxed_std_command(&temp)
        .env("EDITOR", "true")
        .args(["--no-color", "spec", "edit", "zw\u{200B}x"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains(r"Refusing to create 'zw\u{200b}x': it is not a valid spec name"),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{200B}'), "{stderr:?}");
}

// A spec file whose name breaks the rule is named the same way on every line
// of a command that meets it: its row, the summary that names it among what
// could not be answered, and the error that names it.
#[test]
fn an_invalid_spec_name_is_escaped_on_every_line_of_a_run() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    for name in ["z\u{200B}w", "a\u{A0}b"] {
        fs::write(
            packages.join(format!("{name}.yaml")),
            format!("name: zw\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
        )
        .unwrap();
    }

    for args in [
        &["package", "list"][..],
        &["spec", "validate", "--all"][..],
        &["spec", "validate", "z\u{200B}w"][..],
        &["spec", "validate", "a\u{A0}b"][..],
        &["spec", "info", "z\u{200B}w"][..],
        &["spec", "info", "a\u{A0}b"][..],
        &["package", "check", "z\u{200B}w"][..],
        &["package", "check", "a\u{A0}b"][..],
        &["spec", "edit", "z\u{200B}w"][..],
        &["spec", "edit", "a\u{A0}b"][..],
    ] {
        let output = sandboxed_std_command(&temp)
            .env("EDITOR", "true")
            .arg("--no-color")
            .args(args)
            .output()
            .unwrap();
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // A command over every spec names both; one asked about a spec names
        // that one.
        let named: Vec<&str> = match args.last() {
            Some(&"z\u{200B}w") => vec![r"z\u{200b}w"],
            Some(&"a\u{A0}b") => vec![r"a\u{a0}b"],
            _ => vec![r"z\u{200b}w", r"a\u{a0}b"],
        };
        for name in named {
            assert!(both.contains(name), "{args:?} names {name}: {both}");
        }
        for line in both.lines() {
            assert!(
                !line.contains('\u{200B}') && !line.contains('\u{A0}'),
                "{args:?}: {line:?}"
            );
        }
    }
}

// A dependency and an unloadable spec file are named the same way on every
// line of a run: a dependency's status, and the warning listing what was not
// checked for collisions, whose line a line break in a file name must not
// split.
#[test]
fn a_dependency_and_an_unchecked_file_are_escaped_on_every_line() {
    let temp = setup_default_test_config();
    let home = temp.path();
    let packages = home.join("packages");
    fs::write(
        home.join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\nstate_directory: {}\n",
            packages.display(),
            home.join("state").display()
        ),
    )
    .unwrap();
    fs::write(
        packages.join("good.yaml"),
        format!(
            "name: good\ndotfiles:\n  - source: rc\n    target: ~/.rc\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
             dependencies: [\"dep\\u200Bx\"]\n"
        ),
    )
    .unwrap();
    fs::write(packages.join("rc"), "x\n").unwrap();
    for name in ["z\u{200B}w", "l\nf"] {
        fs::write(packages.join(format!("{name}.yaml")), "name: x\n").unwrap();
    }

    for args in [
        &["package", "status", "good"][..],
        &["apply", "--dry-run"][..],
        &["dotfiles", "drift"][..],
    ] {
        let output = sandboxed_std_command(&temp)
            .arg("--no-color")
            .args(args)
            .output()
            .unwrap();
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let named = match args[0] {
            "package" => r"dep\u{200b}x",
            _ => r"packages/l\nf.yaml, packages/z\u{200b}w.yaml",
        };
        assert!(both.contains(named), "{args:?} names {named}: {both}");
        for line in both.lines() {
            assert!(!line.contains('\u{200B}'), "{args:?}: {line:?}");
        }
    }
}

// A blank that is not a space, such as U+00A0 or U+2800, makes one name or path
// read as another, so a listing naming a spec file shows it as an escape.
#[test]
fn a_spec_file_named_with_a_visible_blank_shows_it_escaped() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("a\u{A0}b.yaml"),
        format!("name: \"a\\u00A0b\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "spec", "list"])
        .output()
        .unwrap();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The library's sentence after the path names the spec unescaped, as prose
    // does, so only the path is checked here.
    assert!(both.contains(r"a\u{a0}b.yaml"), "{both}");
    assert!(!both.contains("a\u{A0}b.yaml"), "{both:?}");
}

// A dotfile target holding U+2800 or U+3000 shows each as an escape in the
// plan apply prints.
#[test]
fn a_target_holding_a_visible_blank_shows_it_escaped() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    fs::write(packages.join("blank.conf"), "x\n").unwrap();
    fs::write(
        packages.join("blank.yaml"),
        format!(
            "name: blank\ndotfiles:\n  - source: blank.conf\n    target: \"~/.br\\u2800aille\"\n\
             \x20 - source: blank.conf\n    target: \"~/.ideo\\u3000space\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_std_command(&temp)
        .args(["--no-color", "apply", "--dry-run"])
        .output()
        .unwrap();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        both.contains(r".br\u{2800}aille") && both.contains(r".ideo\u{3000}space"),
        "{both}"
    );
    assert!(!both.contains(['\u{2800}', '\u{3000}']), "{both:?}");
}

// A `command:` dotfile source is a command, so its blanks show as written in
// `dotfiles list` and `spec info`, where a name's escape would change what
// pasting it runs. The blank reads as a space, so the line under the command
// shows it escaped.
#[test]
fn a_command_source_keeps_its_blanks_in_the_dotfiles_list() {
    let temp = setup_default_test_config();
    fs::write(
        temp.path().join("packages").join("cmd.yml"),
        format!(
            "name: cmd\ndotfiles:\n  - command: \"printf 'a\\u00A0b'\"\n    target: \"~/.cmdrc\"\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();

    for args in [&["dotfiles", "list"][..], &["spec", "info", "cmd"][..]] {
        let output = sandboxed_std_command(&temp)
            .arg("--no-color")
            .args(args)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        let lines: Vec<&str> = stdout.lines().collect();
        let command = lines
            .iter()
            .position(|line| line.contains("command: printf 'a\u{A0}b'"))
            .unwrap_or_else(|| panic!("{args:?}: {stdout}"));
        assert!(!lines[command].contains(r"\u{a0}"), "{args:?}: {stdout}");
        assert!(
            lines[command + 1].contains(r"Escaped: printf 'a\u{a0}b'"),
            "{args:?}: {stdout}"
        );
    }
}

// A package directory named `p<U+200B>k`, set as the sandbox's, with `specs`
// written into it as file name and content.
fn hostile_package_directory(temp: &tempfile::TempDir, specs: &[(&str, &str)]) {
    let packages = temp.path().join("p\u{200B}k");
    fs::create_dir_all(&packages).unwrap();
    fs::write(
        temp.path().join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: \"{}\"\n",
            packages.display()
        ),
    )
    .unwrap();
    for (file, spec) in specs {
        fs::write(packages.join(file), spec).unwrap();
    }
}

// A spec named `name` that installs with `true` in the sandbox's environment.
fn plain_spec(name: &str) -> String {
    format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n")
}

// What `args` print on both streams, run in the sandbox without color.
fn printed(temp: &tempfile::TempDir, args: &[&str]) -> String {
    let output = sandboxed_std_command(temp)
        .arg("--no-color")
        .args(args)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// The one line of `text` holding `marker`.
fn line_with<'a>(text: &'a str, marker: &str) -> &'a str {
    let lines: Vec<&str> = text.lines().filter(|line| line.contains(marker)).collect();
    assert_eq!(lines.len(), 1, "{marker:?} in {text}");
    lines[0]
}

// `spec remove`'s scan for dependents skips a spec of an ambiguous name and one
// with an unknown key, and names the directory, the environment and the key
// escaped.
#[test]
fn a_dependent_scans_skipped_specs_name_the_directory_environment_and_key_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[
            ("e.yml", &plain_spec("e")),
            ("m.yml", &plain_spec("m")),
            ("m.yaml", &plain_spec("m")),
            (
                "g.yml",
                "name: g\nenvironments:\n  \"w\\u200Bk\":\n    install: \"true\"\n    \"bo\\u200Bgus\": 1\n",
            ),
        ],
    );

    let printed = printed(&temp, &["spec", "remove", "e", "--yes"]);

    let untrusted = line_with(&printed, "g.yml: selfie will not use the package file");
    assert!(
        untrusted.contains(r"in environment 'w\u{200b}k'"),
        "{untrusted}"
    );
    assert!(
        untrusted.contains(r"unknown field 'bo\u{200b}gus'"),
        "{untrusted}"
    );
    assert!(!untrusted.contains('\u{200B}'), "{untrusted:?}");
    let ambiguous = line_with(
        &printed,
        "m.yml: selfie will not use the package file: Multiple packages found",
    );
    assert!(
        ambiguous.contains(r"p\u{200b}k: m.yaml, m.yml"),
        "{ambiguous}"
    );
    assert!(!ambiguous.contains('\u{200B}'), "{ambiguous:?}");
}

// A not-found failure's hint names the file declaring the name by its
// directory, escaped.
#[test]
fn a_name_field_hint_names_the_declaring_file_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(&temp, &[("bar.yml", &plain_spec("foo"))]);

    let printed = printed(&temp, &["spec", "info", "foo"]);

    let hint = line_with(&printed, "declares `name: foo`");
    assert!(hint.contains(r"p\u{200b}k/bar.yml declares"), "{hint}");
    assert!(!hint.contains('\u{200B}'), "{hint:?}");
}

// `spec validate --all` names the directory of an ambiguous name escaped, as
// `spec list` does.
#[test]
fn spec_validate_all_names_an_ambiguous_names_directory_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[("m.yml", &plain_spec("m")), ("m.yaml", &plain_spec("m"))],
    );

    let printed = printed(&temp, &["spec", "validate", "--all"]);

    let row = line_with(&printed, "Multiple packages found");
    assert!(row.contains(r"p\u{200b}k: m.yaml, m.yml"), "{row}");
    assert!(!row.contains('\u{200B}'), "{row:?}");
}

// A track into a spec carrying a key selfie does not model refuses to rewrite
// it, and names the spec and the key escaped.
#[test]
fn a_refused_spec_save_names_the_spec_and_its_key_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[(
            "c.yml",
            &format!(
                "name: c\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
                 \"a\\u200Budt\": 1\n"
            ),
        )],
    );
    let tracked = temp.path().join(".rc");
    fs::write(&tracked, "x\n").unwrap();

    let printed = printed(
        &temp,
        &["package", "track-dotfile", "c", tracked.to_str().unwrap()],
    );

    let refusal = line_with(&printed, "Cannot save the spec: refusing to rewrite");
    assert!(
        refusal.contains(r"p\u{200b}k/c.yml: unrecognized"),
        "{refusal}"
    );
    assert!(
        refusal.contains(&format!(r"environments.{SELFIE_ENV}.a\u{{200b}}udt")),
        "{refusal}"
    );
    assert!(!refusal.contains('\u{200B}'), "{refusal:?}");
}

// A failed recommend names the failure's file escaped, as the failure would on
// a line of its own.
#[test]
fn a_failed_recommend_names_its_failures_file_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[
            (
                "r.yml",
                &format!(
                    "name: r\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
                     recommends: [b]\n"
                ),
            ),
            ("b.yml", "name: b\nenvironments: [\n"),
        ],
    );

    let printed = printed(&temp, &["package", "install", "r"]);

    let failed = line_with(&printed, "b failed: Parse error in package");
    assert!(failed.contains(r"p\u{200b}k/b.yml"), "{failed}");
    assert!(!failed.contains('\u{200B}'), "{failed:?}");
}

// A dependency that will not load is named, with its file, escaped on one line.
#[test]
fn a_dependency_failure_names_the_failed_dependency_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[
            (
                "d.yml",
                &format!(
                    "name: d\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
                     dependencies: [b]\n"
                ),
            ),
            ("b.yml", "name: b\nenvironments: [\n"),
        ],
    );

    let printed = printed(&temp, &["package", "install", "d"]);

    let failed = line_with(&printed, "Dependency error: Package `d` depends on `b`");
    assert!(failed.contains(r"p\u{200b}k/b.yml"), "{failed}");
    assert!(!failed.contains('\u{200B}'), "{failed:?}");
}

// A dependency the spec names that no spec answers to is named escaped, since
// the name comes from the spec's own text.
#[test]
fn a_missing_dependency_is_named_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[(
            "d.yml",
            &format!(
                "name: d\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
                 dependencies: [\"m\\u200Bx\"]\n"
            ),
        )],
    );

    let printed = printed(&temp, &["package", "install", "d"]);

    let failed = line_with(&printed, "Dependency error: Package `d` depends on");
    assert!(
        failed.contains(r"`m\u{200b}x`, which was not found"),
        "{failed}"
    );
    assert!(!failed.contains('\u{200B}'), "{failed:?}");
}

// Two specs whose entries collide on a target name the target and their
// environment escaped in `spec validate`'s message cell, on one row, with a
// line feed in the target escaped.
#[test]
fn spec_validate_names_a_collisions_target_escaped_on_one_row() {
    let temp = setup_default_test_config();
    let entry = |name: &str| {
        format!(
            "name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    dotfiles:\n\
             \x20     - source: rc\n        target: \"~/t\\u200Bx\\nl\"\n"
        )
    };
    hostile_package_directory(&temp, &[("a.yml", &entry("a")), ("s.yml", &entry("s"))]);

    for args in [
        &["spec", "validate", "a"][..],
        &["spec", "validate", "--all"][..],
    ] {
        let printed = printed(&temp, args);

        let rows: Vec<&str> = printed
            .lines()
            .filter(|line| line.contains("Dotfile entries package 'a'"))
            .collect();
        assert!(!rows.is_empty(), "{args:?}: {printed}");
        for row in rows {
            assert!(row.contains(r"~/t\u{200b}x\nl"), "{args:?}: {row}");
            assert!(
                row.contains("so apply deploys none of them there"),
                "{args:?}: {row}"
            );
            assert!(!row.contains('\u{200B}'), "{args:?}: {row:?}");
        }
    }
}

// A push refused for files claiming one name names each file escaped, so a
// U+200B shows and a line feed keeps the refusal on its table row.
#[test]
fn sync_push_names_files_claiming_one_name_escaped_on_one_row() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    for file in ["n\u{200B}v\nx.yml", "n\u{200B}v\nx.yaml"] {
        fs::write(packages.join(file), plain_spec("nv")).unwrap();
    }
    package_repo_with_remote(&temp);
    fs::write(packages.join("o.yml"), plain_spec("o")).unwrap();

    let printed = printed(&temp, &["sync", "push", "--yes"]);

    let row = line_with(&printed, "name the same package under different extensions");
    assert!(
        row.contains(r"n\u{200b}v\nx.yaml and n\u{200b}v\nx.yml name the same package"),
        "{row}"
    );
    assert!(!printed.contains('\u{200B}'), "{printed:?}");
}

// A spec whose `name:` field does not match its file name names it escaped, and
// the suggestion beside it does not repeat it.
#[test]
fn spec_validate_names_a_mismatched_name_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(
        &temp,
        &[(
            "n.yml",
            &format!(
                "name: \"n\\u200Bm\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
            ),
        )],
    );

    let printed = printed(&temp, &["spec", "validate", "n"]);

    let row = line_with(&printed, "does not match filename");
    assert!(row.contains(r"Package name 'n\u{200b}m'"), "{row}");
    assert!(!row.contains('\u{200B}'), "{row:?}");
}

// Every command that names a refused spec's reason shows its unknown key, and
// the environment the key sits in, escaped: a listing's row, a run's skipping
// line, a single-package command's failure, and spec info's refusal here and
// in another environment. A spec that does not parse is skipped with its file
// named escaped, and each listing names an ambiguity's directory escaped.
#[test]
fn a_refused_specs_reason_names_its_key_and_environment_escaped() {
    let temp = setup_default_test_config();
    let keyed = format!(
        "name: g\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \"w\\u200Bk\": 1\n"
    );
    let elsewhere = "name: e\nenvironments:\n  \"w\\u200Bv\":\n    install: \"true\"\n    \
                     \"x\\u200By\": 1\n";
    hostile_package_directory(
        &temp,
        &[
            ("g.yml", &keyed),
            ("e.yml", elsewhere),
            ("b.yml", "name: b\n  bad: [\n"),
            ("m.yml", &plain_spec("m")),
            ("m.yaml", &plain_spec("m")),
        ],
    );
    let ambiguity = r"p\u{200b}k: m.yaml, m.yml";
    let key = r"unknown field 'w\u{200b}k'";
    let cases: [(&[&str], &str, &str); 12] = [
        (&["package", "list"], "refused (g.yml)", key),
        (&["spec", "list"], "Refused: g", key),
        (&["apply"], "Skipping package 'g'", key),
        (&["dotfiles", "drift"], "Skipping package 'g'", key),
        (&["package", "check", "g"], "Cannot use package `g`", key),
        (&["spec", "info", "g"], "would refuse this spec here", key),
        (
            &["dotfiles", "list"],
            "Cannot read the dotfiles in 'e'",
            r"in environment 'w\u{200b}v': unknown field 'x\u{200b}y'",
        ),
        (&["apply"], "Skipping package file", r"p\u{200b}k/b.yml"),
        (
            &["spec", "info", "e"],
            "would refuse this spec in another environment",
            r"in environment 'w\u{200b}v': unknown field 'x\u{200b}y'",
        ),
        (&["package", "list"], "refused (m.yaml)", ambiguity),
        (&["spec", "list"], "m.yaml) — Multiple packages", ambiguity),
        (
            &["dotfiles", "list"],
            "m.yaml): Multiple packages",
            ambiguity,
        ),
    ];

    for (args, marker, escaped) in cases {
        let printed = printed(&temp, args);
        let line = line_with(&printed, marker);
        assert!(line.contains(escaped), "{args:?}: {line}");
        assert!(!line.contains('\u{200B}'), "{args:?}: {line:?}");
    }
}

// A not-found failure's hint shows the declared `name:` value as a name: an
// invisible character escaped once, and a quote as it is written.
#[test]
fn a_name_field_hint_shows_the_declared_name_as_a_name() {
    for (declared, asked, shown) in [
        ("ze\\u200Bo", "ze\u{200B}o", r"name: ze\u{200b}o`"),
        ("ze\\\"o", "ze\"o", "name: ze\"o`"),
    ] {
        let temp = setup_default_test_config();
        let spec = format!(
            "name: \"{declared}\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        );
        hostile_package_directory(&temp, &[("bar.yml", &spec)]);

        let printed = printed(&temp, &["apply", asked]);

        let hint = line_with(&printed, "bar.yml declares");
        assert!(hint.contains(shown), "{asked:?}: {hint}");
        assert!(!hint.contains('\u{200B}'), "{asked:?}: {hint:?}");
        assert!(hint.contains(r"p\u{200b}k/bar.yml"), "{asked:?}: {hint}");
    }
}

// A package directory outside any git repository is named escaped, by the
// discovery step of every sync command that starts from it.
#[test]
fn a_sync_outside_a_repository_names_the_directory_escaped() {
    let temp = setup_default_test_config();
    hostile_package_directory(&temp, &[("a.yml", &plain_spec("a"))]);
    // Discovery walks up from the directory, so a TMPDIR inside a work tree
    // would find that one.
    let inside = common::isolated_git()
        .arg("-C")
        .arg(temp.path().join("p\u{200B}k"))
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "the sandbox {} is inside a git work tree, so this test cannot run here",
        temp.path().display()
    );

    // Push's discovery runs in its preparation, which words the failure as the
    // error alone, with no frame.
    for args in [
        &["sync", "status"][..],
        &["sync", "pull"][..],
        &["sync", "push", "--yes"][..],
    ] {
        let printed = printed(&temp, args);
        let line = line_with(&printed, "not a git repository: ");
        assert!(line.ends_with(r"/p\u{200b}k"), "{args:?}: {line}");
        assert!(!line.contains('\u{200B}'), "{args:?}: {line:?}");
    }
}

// git's own text, naming a directory in the repository it could not read, shows
// that directory escaped: in sync status's failure, sync push's, and the
// warning spec info and spec list give when git status is unavailable.
#[test]
#[cfg(unix)]
fn gits_own_text_names_a_directory_escaped() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = setup_default_test_config();
    let repo = temp.path().join("repo");
    let packages = repo.join("p\u{200B}k");
    fs::create_dir_all(&packages).unwrap();
    common::git(&repo, &["init", "-q"]);
    fs::write(packages.join("a.yml"), plain_spec("a")).unwrap();
    fs::write(
        temp.path().join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: \"{}\"\n",
            packages.display()
        ),
    )
    .unwrap();
    let unreadable = repo.join("q\u{200B}r");
    fs::create_dir_all(&unreadable).unwrap();
    fs::write(unreadable.join("f"), "x").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    // Root, or a file system that ignores modes, reads it anyway, and then git
    // does not fail.
    if fs::read_dir(&unreadable).is_ok() {
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("SKIP: mode 0o000 is not honored here, so the directory stays readable");
        return;
    }

    // Each line is found by selfie's own words where it has any, and by the
    // escaped directory where git's text is all there is, never by git's or
    // gix's wording, which changes with a release or a locale.
    let cases: [(&[&str], &str); 4] = [
        (&["sync", "status"], "Failed to get repo status: "),
        (&["sync", "push", "--yes"], r"/q\u{200b}r"),
        (&["spec", "info", "a"], "Git status unavailable: "),
        (&["spec", "list"], "Git status unavailable: "),
    ];
    // git's own messages in the C locale, so a translated one cannot change
    // which line holds the directory.
    let printed = |args: &[&str]| {
        let output = sandboxed_std_command(&temp)
            .env("LC_ALL", "C")
            .arg("--no-color")
            .args(args)
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let shown: Vec<String> = cases
        .iter()
        .map(|(args, marker)| line_with(&printed(args), marker).to_string())
        .collect();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();

    for ((args, _), line) in cases.iter().zip(&shown) {
        assert!(line.contains(r"/q\u{200b}r"), "{args:?}: {line}");
        assert!(!line.contains('\u{200B}'), "{args:?}: {line:?}");
    }
}

// A write tempfile fails names the temporary file in tempfile's own text, and
// that path shows its blank escaped too, not only the target's. U+2800 is the
// blank tempfile's own `{:?}` leaves as it is, so only the CLI escapes it.
#[test]
#[cfg(unix)]
fn a_failed_temporary_files_path_shows_escaped() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = setup_default_test_config();
    let packages = temp.path().join("pk");
    fs::create_dir_all(&packages).unwrap();
    fs::write(packages.join("rc"), "hi\n").unwrap();
    fs::write(
        packages.join("a.yml"),
        format!(
            "name: a\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  - \
             source: rc\n    target: \"~/r\\u2800o/f\"\n"
        ),
    )
    .unwrap();
    fs::write(
        temp.path().join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: \"{}\"\n",
            packages.display()
        ),
    )
    .unwrap();
    let locked = temp.path().join("r\u{2800}o");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    // Root writes into it anyway, and then nothing fails.
    if fs::write(locked.join("probe"), "x").is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("SKIP: running as root, so the directory is writable");
        return;
    }

    let printed = printed(&temp, &["apply"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let line = line_with(&printed, " at path ");
    assert!(line.contains(r#"r\u{2800}o/.selfie-"#), "{line}");
    assert!(!line.contains('\u{2800}'), "{line:?}");
}

// A spec's command inside a failure shows on its line as written, so it still
// pastes, and the line after it spells out the character that reads as
// another: a check that timed out, in its failure and in package list's
// status cell, and an install that failed.
#[test]
fn a_spec_command_in_a_failure_is_noted() {
    let temp = setup_default_test_config();
    let spec = |name: &str, commands: &str| {
        format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n{commands}")
    };
    hostile_package_directory(
        &temp,
        &[
            (
                "s.yml",
                &spec(
                    "s",
                    "    install: \"true\"\n    check: \"sleep 5 #\\u200Bs\"\n",
                ),
            ),
            ("f.yml", &spec("f", "    install: \"false #\\u200Bf\"\n")),
        ],
    );
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut settings = fs::read_to_string(&config).unwrap();
    settings.push_str("command_timeout: 1\n");
    fs::write(&config, settings).unwrap();

    let cases: [(&[&str], &str, &str); 3] = [
        (
            &["package", "check", "s"],
            "timed out after 1 second",
            r"sleep 5 #\u{200b}s",
        ),
        (
            &["package", "list"],
            "timed out after 1 second",
            r"sleep 5 #\u{200b}s",
        ),
        (
            &["package", "install", "f"],
            "Command error: Command `",
            r"false #\u{200b}f",
        ),
    ];
    for (args, marker, escaped) in cases {
        let printed = printed(&temp, args);
        let lines: Vec<&str> = printed.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.contains(marker))
            .unwrap_or_else(|| panic!("{args:?}: {printed}"));
        assert!(
            lines[at + 1].contains(&format!("Escaped: {escaped}")),
            "{args:?}: {printed}"
        );
    }

    // The note is a line of the status cell, so every line of the table is as
    // wide as the rest. The command shows its U+200B as written, at no width.
    let listed = printed(&temp, &["package", "list"]);
    let widths: std::collections::BTreeSet<usize> = listed
        .lines()
        .filter(|line| line.starts_with(['╭', '│', '╞', '╰']))
        .map(|line| line.chars().filter(|&c| c != '\u{200B}').count())
        .collect();
    assert_eq!(widths.len(), 1, "{listed}");
}

// package status shows a status selfie could not answer for after "Unknown: ",
// and a timed-out check's command noted on the line under it: in the
// package's own status row, and in its dependency list, where each dependency
// has its own line, so the note stays under the one it explains. A refused
// dependency's reason names its key escaped.
#[test]
fn a_status_selfie_could_not_answer_for_is_noted_under_its_line() {
    let temp = setup_default_test_config();
    let spec = |name: &str, commands: &str| {
        format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n{commands}")
    };
    hostile_package_directory(
        &temp,
        &[
            (
                "x.yml",
                &spec(
                    "x",
                    "    install: \"true\"\n    check: \"sleep 5 #\\u200Bx\"\n",
                ),
            ),
            (
                "y.yml",
                &spec("y", "    install: \"true\"\n    check: \"true\"\n"),
            ),
            (
                "two.yml",
                &spec(
                    "two",
                    "    install: \"true\"\n    check: \"true\"\n    dependencies: [x, y]\n",
                ),
            ),
            (
                "g.yml",
                &spec("g", "    install: \"true\"\n    \"k\\u200By\": 1\n"),
            ),
            (
                "dg.yml",
                &spec(
                    "dg",
                    "    install: \"true\"\n    check: \"true\"\n    dependencies: [g]\n",
                ),
            ),
        ],
    );
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut settings = fs::read_to_string(&config).unwrap();
    settings.push_str("command_timeout: 1\n");
    fs::write(&config, settings).unwrap();

    let printed_two = printed(&temp, &["package", "status", "two"]);
    let lines: Vec<&str> = printed_two.lines().collect();
    let x = lines
        .iter()
        .position(|line| line.contains("x: Unknown: `sleep 5 #"))
        .unwrap_or_else(|| panic!("{printed_two}"));
    assert!(
        lines[x + 1].contains(r"Escaped: sleep 5 #\u{200b}x"),
        "{printed_two}"
    );
    assert!(lines[x + 2].contains("y: "), "{printed_two}");

    let printed_dg = printed(&temp, &["package", "status", "dg"]);
    let g = line_with(&printed_dg, "g: Unknown: is refused:");
    assert!(g.contains(r"unknown field 'k\u{200b}y'"), "{g}");
    assert!(!g.contains('\u{200B}'), "{g:?}");
}

// A recommend whose check gives no answer after its install warns with the
// check's command noted on the line under the warning.
#[test]
fn a_recommends_unanswered_check_is_noted() {
    let temp = setup_default_test_config();
    let spec = |name: &str, commands: &str| {
        format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n{commands}")
    };
    hostile_package_directory(
        &temp,
        &[
            (
                "s.yml",
                &spec(
                    "s",
                    "    install: \"true\"\n    check: \"sleep 5 #\\u200Bs\"\n",
                ),
            ),
            (
                "r.yml",
                &spec("r", "    install: \"true\"\n    recommends: [s]\n"),
            ),
        ],
    );
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut settings = fs::read_to_string(&config).unwrap();
    settings.push_str("command_timeout: 1\n");
    fs::write(&config, settings).unwrap();

    let printed = printed(&temp, &["package", "install", "r"]);
    let lines: Vec<&str> = printed.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains("installed, but its check gave no answer: "))
        .unwrap_or_else(|| panic!("{printed}"));
    assert!(
        lines[at + 1].contains(r"Escaped: sleep 5 #\u{200b}s"),
        "{printed}"
    );
}
