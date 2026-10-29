#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A non-ASCII identifier is a capture too.
fn main() {
    let é = String::from("x");
    let _ = shown!("{é}");
}
