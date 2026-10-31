#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;
#[path = "../../src/sentence.rs"]
mod sentence;

// A name held in a String is not a library sentence, so it cannot be shown as
// one by passing it where the library's values go.
fn main() {
    let name = String::from("tool\u{200b}");
    let _ = sentence::sentence(&name);
    let _ = sentence::sentence("tool");
}
