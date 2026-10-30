#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A width argument is read without the Trusted check.
fn main() {
    let w = 3usize;
    let _ = shown!("{:w$}", 1u8);
}
