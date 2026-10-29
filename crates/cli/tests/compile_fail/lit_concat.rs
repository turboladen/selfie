#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A macro call is not a string-literal token.
fn main() {
    let _ = lit!(concat!("a", "b"));
}
