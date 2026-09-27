//! A small target for `cargo xtask mutate --self-test`, which mutates this
//! file in its own archive. The tests pass as committed.

fn answer() -> u32 {
    // The comment-only control edits this line and nothing else.
    42
}

fn returns_promptly() {}

#[test]
fn the_answer_is_42() {
    assert_eq!(answer(), 42);
}

#[test]
fn a_call_returns() {
    returns_promptly();
}
