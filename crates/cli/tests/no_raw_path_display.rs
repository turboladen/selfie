// A path reaches the terminal only through `DisplayPath`, which escapes what a
// file name could use to act on the terminal or pass for another name. A
// `.display()` anywhere else in the CLI's source would print a path raw.
//
// A clippy ban would also fire in the tests, where paths are formatted into
// fixtures by the hundred, and allowing it there module by module would silence
// the CLI's other bans with it. So this reads the source instead, skipping the
// `#[cfg(test)]` modules.

use std::path::{Path, PathBuf};

// Where `.display()` may appear outside tests: `DisplayPath::of`, the one place
// a path's text is taken for display.
const ALLOWED: &[(&str, &str)] = &[("terminal_text.rs", "Self::new(&path.display().to_string())")];

#[test]
fn no_path_is_displayed_except_through_display_path() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    assert!(files.len() > 20, "the walk found too few files: {files:?}");

    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        for (number, line) in outside_test_modules(&text) {
            let code = line.trim_start();
            if code.starts_with("//") || !line.contains(".display()") {
                continue;
            }
            let allowed = ALLOWED
                .iter()
                .any(|(file, text)| *file == name && line.contains(text));
            if !allowed {
                found.push(format!("{}:{number}: {}", file.to_string_lossy(), code));
            }
        }
    }

    assert!(
        found.is_empty(),
        "render these paths with DisplayPath::of or shorten_path:\n{}",
        found.join("\n")
    );
}

// A table reaches the terminal only through `SafeTable`, which escapes every
// cell not marked as selfie's own styling, so only tables.rs may build a
// comfy-table `Table` directly.
#[test]
fn no_table_is_built_except_through_safe_table() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);

    let found: Vec<String> = files
        .iter()
        .filter(|file| **file != src.join("tables.rs"))
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).unwrap();
            outside_test_modules(&text)
                .filter(|(_, line)| line.contains("comfy_table"))
                .map(|(number, line)| {
                    format!("{}:{number}: {}", file.to_string_lossy(), line.trim())
                })
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        found.is_empty(),
        "build these tables with tables::SafeTable:\n{}",
        found.join("\n")
    );
}

// Text reaches a styled printer only as a `Shown`. `shown!` builds one through
// `Shown::__assembled` after checking each argument, and `lit!` builds a
// `Literal` through `Literal::__from_literal` from a string-literal token. Both
// are callable from any file, since a macro's expansion is checked for privacy
// where it is used, so a direct call elsewhere skips the check.
//
// This matches the names as text, test modules included. A call has to name
// the function, an alias cannot rename an associated function, and a comment
// or string naming one fails the test rather than hiding a call.
const HIDDEN: &[&str] = &["__assembled", "__from_literal"];

#[test]
fn hidden_constructors_are_called_only_in_terminal_text() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    assert!(files.len() > 20, "the walk found too few files: {files:?}");
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("commands/package/check.rs")),
        "the walk did not reach nested modules: {files:?}"
    );

    let found: Vec<String> = files
        .iter()
        .filter(|file| **file != src.join("terminal_text.rs"))
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).unwrap();
            hidden_calls(&text)
                .into_iter()
                .map(|(number, line)| format!("{}:{number}: {}", file.to_string_lossy(), line))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        found.is_empty(),
        "build these with shown! or lit!:\n{}",
        found.join("\n")
    );
}

// The scan must see what it is meant to forbid: a direct call, a raw
// identifier, and a call through an alias of the type.
#[test]
fn the_hidden_constructor_scan_sees_a_direct_call() {
    let text = "use crate::terminal_text::{Literal as L, Shown};\n\
                fn a() -> Shown { Shown::__assembled(String::new()) }\n\
                fn b() -> L { L::r#__from_literal(leaked()) }\n\
                fn c() -> Shown { shown!(\"{}\", lit!(\"x\")) }\n";

    let lines: Vec<usize> = hidden_calls(text)
        .into_iter()
        .map(|(number, _)| number)
        .collect();

    assert_eq!(lines, [2, 3]);
}

// The lines of `text`, numbered from 1, that name a hidden constructor.
fn hidden_calls(text: &str) -> Vec<(usize, &str)> {
    lines_naming(text, HIDDEN)
}

// The lines of `text`, numbered from 1, that hold any of `names`.
fn lines_naming<'a>(text: &'a str, names: &[&str]) -> Vec<(usize, &'a str)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| names.iter().any(|name| line.contains(name)))
        .map(|(index, line)| (index + 1, line.trim()))
        .collect()
}

// A text's continuation lines are indented by the layout that places it, at
// the column where it starts: the message printers, a table and the entry
// rows. `Shown::hang` is how they indent, so only their modules, and the one
// that defines it, may call it.
//
// This guards that one method, matched as text like the hidden constructors.
// It does not see a command that writes its own indent after a line break into
// a `lit!` or `shown!` literal.
const HANG: &[&str] = &[".hang("];
const LAYOUT_MODULES: &[&str] = &[
    "terminal_text.rs",
    "display_manager.rs",
    "tables.rs",
    "entry_rows.rs",
];

#[test]
fn hang_is_called_only_in_the_layout_modules() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    assert!(
        LAYOUT_MODULES
            .iter()
            .all(|module| files.contains(&src.join(module))),
        "a layout module moved: {files:?}"
    );

    let found: Vec<String> = files
        .iter()
        .filter(|file| {
            !LAYOUT_MODULES
                .iter()
                .any(|module| **file == src.join(module))
        })
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).unwrap();
            lines_naming(&text, HANG)
                .into_iter()
                .map(|(number, line)| format!("{}:{number}: {}", file.to_string_lossy(), line))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        found.is_empty(),
        "print these through a layout, such as DisplayManager::println_after:\n{}",
        found.join("\n")
    );
}

// The scan must see a call, chained or not.
#[test]
fn the_hang_scan_sees_a_call() {
    let text = "fn a(s: Shown) -> Shown { s.hang(2) }\n\
                fn b(s: Shown) -> Shown { s.paint(true, red)\n    .hang(4) }\n\
                fn c(s: Shown) -> Shown { s.change(2) }\n";

    let lines: Vec<usize> = lines_naming(text, HANG)
        .into_iter()
        .map(|(number, _)| number)
        .collect();

    assert_eq!(lines, [1, 3]);
}

// The gate must see what it is meant to forbid: a `.display()` outside a test
// module is reported, and one inside it is not.
#[test]
fn the_gate_sees_a_display_outside_a_test_module() {
    let text = "fn shown(p: &Path) -> String { p.display().to_string() }\n\
                #[cfg(test)]\nmod tests {\n    fn fixture(p: &Path) { p.display(); }\n}\n";

    let lines: Vec<usize> = outside_test_modules(text)
        .filter(|(_, line)| line.contains(".display()"))
        .map(|(number, _)| number)
        .collect();

    assert_eq!(lines, [1]);
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

// The lines of `text`, numbered from 1, outside every `#[cfg(test)] mod`. A
// module's end is found by counting braces, which the CLI's test modules keep
// balanced.
fn outside_test_modules(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut depth: i64 = 0;
    let mut in_test = false;
    let mut attribute = false;
    text.lines().enumerate().filter_map(move |(index, line)| {
        let code = line.trim();
        if in_test {
            depth += brace_balance(line);
            if depth <= 0 {
                in_test = false;
            }
            return None;
        }
        if code.starts_with("#[cfg(test)]") {
            attribute = true;
            return None;
        }
        if attribute {
            if code.starts_with("mod ") && code.ends_with('{') {
                in_test = true;
                depth = brace_balance(line);
                attribute = false;
                return None;
            }
            if !code.starts_with("#[") {
                attribute = false;
            }
        }
        Some((index + 1, line))
    })
}

fn brace_balance(line: &str) -> i64 {
    line.chars().fold(0, |depth, c| match c {
        '{' => depth + 1,
        '}' => depth - 1,
        _ => depth,
    })
}
