#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;

use terminal_text::{Literal, Shown, lit, shown};

// selfie's own text compiles as a Literal, in a const too, and so do escaped
// braces in the format and a value of every number type.
const SEPARATOR: Literal = lit!("--");

fn main() {
    let _ = shown!("{{}} {} {} {}", SEPARATOR, lit!("x"), 1u8);
    let _ = shown!(
        "{} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        1u8, 1u16, 1u32, 1u64, 1u128, 1usize, 1i8, 1i16, 1i32, 1i64, 1i128, 1isize, 1.0f32,
        1.0f64
    );
    let _ = Shown::from(lit!("y"));
}
