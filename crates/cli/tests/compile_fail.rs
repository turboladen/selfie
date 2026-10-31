// What `terminal_text.rs` refuses at compile time: text from outside selfie
// formatted by `shown!` without being escaped, a `shown!` placeholder other
// than `{}`, a type outside the module marked as Trusted, a `Literal` made from
// anything but a string literal, a kind built around text its constructor
// never escaped, and a String handed to the bound the printers take. Also what
// `sentence.rs` refuses: a string passed where the library's values go. Each
// case includes the modules by path, since selfie-cli has no library target to
// depend on, and the stand-in tables module they name.
//
// The `.stderr` files hold rustc's wording, which a new toolchain can change.
// After `rustup update stable`, regenerate them with `TRYBUILD=overwrite`.
// rustc lists the first few implementors of a trait it found unimplemented, so
// a new `Sentence` impl that sorts among them rewrites
// `sentence_takes_no_string.stderr` too.

#[test]
fn misuse_of_terminal_text_does_not_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/compile_pass/*.rs");
    cases.compile_fail("tests/compile_fail/*.rs");
}
