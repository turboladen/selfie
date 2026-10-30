#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::lit;

// A literal of another type is not text.
fn main() {
    let _ = lit!(1);
}
