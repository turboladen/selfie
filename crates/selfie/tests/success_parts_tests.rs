// Each value a success summary names is the part kind it is: the environment
// and the package are names, the path is a path, and no text holds any of
// them. A CLI escapes a name and a path alike today, so only this test tells
// a name shown as a path from one shown as a name.

use selfie::parts::{Composed, Part};

const ENVIRONMENT: &str = "ENVIRONMENT";
const PACKAGE: &str = "PACKAGE";
const PATH: &str = "/PATH";

#[test]
fn every_value_a_success_summary_names_is_its_kind() {
    let mut named = [0; 3];
    for success in test_common::every_success(ENVIRONMENT, PACKAGE, PATH) {
        for part in &success.parts() {
            match part {
                Part::Name(name) => {
                    assert!(
                        [ENVIRONMENT, PACKAGE].contains(name),
                        "{name:?} in {success:?}"
                    );
                    named[usize::from(*name == PACKAGE)] += 1;
                }
                Part::Path(path) => {
                    assert_eq!(*path, std::path::Path::new(PATH), "{success:?}");
                    named[2] += 1;
                }
                Part::Text(text) => assert!(
                    ![ENVIRONMENT, PACKAGE, PATH]
                        .iter()
                        .any(|value| text.contains(value)),
                    "{text:?} in {success:?}"
                ),
            }
        }
    }
    // Each kind of value is named somewhere, so the assertions above judge
    // something.
    assert!(named.iter().all(|count| *count > 0), "{named:?}");
}
