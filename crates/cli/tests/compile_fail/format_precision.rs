#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A precision is a format spec too.
fn main() {
    let _ = shown!("{:.2}", 1.0f64);
}
