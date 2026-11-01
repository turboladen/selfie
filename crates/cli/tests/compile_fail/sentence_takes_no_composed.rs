#![allow(dead_code, unused)]

#[path = "../../src/terminal_text.rs"]
mod terminal_text;
#[path = "../compile_support/tables.rs"]
mod tables;
#[path = "../../src/sentence.rs"]
mod sentence;

// A success summary is built from parts, so it is shown through composed(),
// which escapes each name in it as a name, and never as one prose sentence.
fn main() {
    let done = selfie::package::event::OperationSuccess::Generic("done".to_string());
    let _ = sentence::sentence(&done);
}
