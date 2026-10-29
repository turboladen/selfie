#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// An index placeholder is refused, so every placeholder is a plain {}.
fn main() {
    let _ = shown!("{0}", lit!("x"));
}
