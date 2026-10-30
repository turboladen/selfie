#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// Stands in for a printer: the CLI's printers and cells take `impl Into<Shown>`,
// which a kind satisfies and a String does not. This pins what that bound
// refuses, not that every printer keeps it: a case cannot include
// display_manager.rs, which needs the whole CLI to build.
fn print(_: impl Into<Shown>) {}

fn main() {
    print(format!("{}", "x"));
}
