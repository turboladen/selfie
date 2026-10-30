#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A `&'static str` can be leaked from a String, so lit! takes only a literal.
fn main() {
    let leaked: &'static str = String::from("x").leak();
    let _ = lit!(leaked);
}
