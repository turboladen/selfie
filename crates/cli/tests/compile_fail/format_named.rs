#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// An inline name captures a variable that was never checked as Trusted.
fn main() {
    let name = String::from("x");
    let _ = shown!("{name}");
}
