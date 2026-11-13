use selfie::package::event::OperationSuccess;

// A generic success holds only a literal, so a sentence built from a value,
// such as a path, cannot reach the CLI as prose. A success naming a value
// needs an arm of its own, whose sentence names it as a part.
fn main() {
    let path = "/home/u/.gitconfig";
    let _ = OperationSuccess::Generic(format!("Removed {path}"));
    let _: OperationSuccess = format!("Removed {path}").into();
}
