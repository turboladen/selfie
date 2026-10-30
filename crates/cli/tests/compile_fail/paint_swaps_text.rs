#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Shown, lit};

// A paint styles the text it is given; it cannot return other text in its
// place, which would reach the terminal unescaped.
fn main() {
    let _ = Shown::from(lit!("x")).paint(true, |_| console::style(String::from("\u{1b}[2J")));
}
