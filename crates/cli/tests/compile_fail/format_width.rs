#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A format spec is refused, even one with no argument of its own.
fn main() {
    let _ = shown!("{:>4}", 1u8);
}
