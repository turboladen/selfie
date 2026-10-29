#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// A leaked `&'static str` is text like any other, so shown! refuses it.
fn main() {
    let leaked: &'static str = String::from("x").leak();
    let _ = shown!("{}", leaked);
}
