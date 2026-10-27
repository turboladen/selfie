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
