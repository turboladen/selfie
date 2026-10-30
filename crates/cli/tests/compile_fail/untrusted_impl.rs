#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Trusted, shown};

// Only terminal_text.rs implements Trusted, so no other module can mark text
// of its own as safe to print.
struct Raw(String);

impl std::fmt::Display for Raw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Trusted for Raw {}

fn main() {
    let _ = shown!("{}", Raw(String::from("\u{1b}[2J")));
}
