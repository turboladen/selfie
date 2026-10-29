#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// selfie's own text compiles as a Literal, in a const too, and so do numbers
// and escaped braces in the format.
const SEPARATOR: Literal = lit!("--");

fn main() {
    let _ = shown!("{{}} {} {} {}", SEPARATOR, lit!("x"), 1u8);
    let _ = Shown::from(lit!("y"));
}
