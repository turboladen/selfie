#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A Shown is not made from a bare string, leaked or not.
fn main() {
    let leaked: &'static str = String::from("x").leak();
    let _ = Shown::from(leaked);
}
