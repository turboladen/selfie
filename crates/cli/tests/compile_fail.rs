// What `terminal_text.rs` and `sentence.rs` refuse at compile time: text from
// outside selfie formatted by `shown!` unescaped, a placeholder other than `{}`,
// a foreign type marked Trusted, a `Literal` from anything but a literal, a kind
// built around unescaped text, a String handed to the printers' bound (which
// source_rules.rs holds every printer to), and a string passed to `sentence`.
// Each case includes the modules by path, as selfie-cli has no library target.
//
// The `.stderr` files hold rustc's wording; after `rustup update stable`, or a
// new `Sentence` impl that sorts among the implementors rustc lists, regenerate
// them with `TRYBUILD=overwrite`.

#[test]
fn misuse_of_terminal_text_does_not_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/compile_pass/*.rs");
    cases.compile_fail("tests/compile_fail/*.rs");
}
