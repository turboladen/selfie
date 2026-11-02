//! Messages for tests that build an event the library would send.

use std::path::Path;

use selfie::parts::{ForeignText, Message, Part, Parts, SpecCommandText, SuggestedCommand};

/// A message whose sentence is `text` alone.
#[must_use]
pub fn message(text: &str) -> Message {
    built(vec![Part::Text(text.to_string().into())])
}

// The message `parts` make, in order.
fn built(parts: Vec<Part<'_>>) -> Message {
    parts.into_iter().collect::<Parts>().into()
}

/// Messages holding every kind of part, each name, path, foreign text and
/// spec command naming `value`, with a line break at the end of one and inside another.
///
/// # Panics
///
/// Panics when `value` holds NUL, which no suggested command can carry.
#[must_use]
pub fn every_message(value: &str) -> Vec<Message> {
    let command = SuggestedCommand::make_directory(Path::new(value)).expect("no NUL");
    vec![
        message("plain"),
        message("two\nlines"),
        message("ends\n"),
        built(vec![
            Part::Text("Skipping '".into()),
            Part::Path(Path::new(value).into()),
            Part::Text("' in package '".into()),
            Part::Name(value.into()),
            Part::Text("'".into()),
        ]),
        built(vec![
            Part::Text("Create it with: ".into()),
            Part::Command(command),
        ]),
        built(vec![
            Part::Text("git says: ".into()),
            Part::Foreign(ForeignText::for_tests(value)),
        ]),
        built(vec![
            Part::Text("Run ".into()),
            Part::SpecCommand(SpecCommandText::for_tests(value)),
        ]),
    ]
}
