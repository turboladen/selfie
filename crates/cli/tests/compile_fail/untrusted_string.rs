#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A String was never escaped.
fn main() {
    let _ = shown!("{}", String::from("x"));
}
