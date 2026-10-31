#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// Stands in for a printer: the CLI's printers and cells take `impl Into<Shown>`,
// which a kind satisfies and a String does not. This pins what that bound
// refuses. A case cannot include display_manager.rs, which needs the whole CLI
// to build, so tests/source_rules.rs is what holds every printer to the bound.
fn print(_: impl Into<Shown>) {}

fn main() {
    print(format!("{}", "x"));
}
