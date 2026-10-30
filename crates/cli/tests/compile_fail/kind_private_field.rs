#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::Name;

// A kind holds its escaped text, so only its constructor builds one: its field
// is private to the module.
fn main() {
    let _ = Name { 0: String::from("a\u{1b}[2Jb") };
}
