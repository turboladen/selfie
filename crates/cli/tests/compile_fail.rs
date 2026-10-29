// What `terminal_text.rs` refuses at compile time: text from outside selfie
// formatted by `shown!` without being escaped, a type outside the module marked
// as Trusted, and a `Literal` made from anything but a string literal. Each
// case includes the module by path, since selfie-cli has no library target to
// depend on, and the stand-in tables module it names.
//
// The `.stderr` files hold rustc's wording, which a new toolchain can change.
// After `rustup update stable`, regenerate them with `TRYBUILD=overwrite`.

#[test]
fn misuse_of_terminal_text_does_not_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/compile_pass/*.rs");
    cases.compile_fail("tests/compile_fail/*.rs");
}
