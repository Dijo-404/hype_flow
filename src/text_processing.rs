//! Deterministic, opt-in spoken punctuation. Protected spans are handled by cleanup.
use regex::{Captures, Regex};
use std::sync::LazyLock;

static COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?P<command>question mark|exclamation (?:mark|point)|full stop|semicolon|comma|colon)\b(?:[ \t]*[,.!?;:])*").unwrap()
});

pub fn whole_command(text: &str) -> Option<&'static str> {
    match text
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "new line" => Some("\n"),
        "new paragraph" => Some("\n\n"),
        "period" | "full stop" => Some("."),
        "comma" => Some(","),
        "question mark" => Some("?"),
        "exclamation mark" | "exclamation point" => Some("!"),
        // This retracts the current utterance only, never text in another application.
        "scratch that" => Some(""),
        _ => None,
    }
}

pub fn spoken_punctuation(text: &str) -> String {
    COMMANDS
        .replace_all(text, |captures: &Captures<'_>| {
            let command = captures["command"].to_ascii_lowercase();
            match command.as_str() {
                "question mark" => "?",
                "exclamation mark" | "exclamation point" => "!",
                "full stop" => ".",
                "comma" => ",",
                "colon" => ":",
                "semicolon" => ";",
                _ => unreachable!(),
            }
        })
        .into_owned()
}

/// Capitalize sentence starts without changing mixed-case identifiers or acronyms.
pub fn capitalize_sentences(text: &str, protected: &[(usize, usize)]) -> String {
    static STARTS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[.!?\n]\s+)(?P<word>\p{L}[\p{L}\p{N}_-]*)").unwrap());
    STARTS
        .replace_all(text, |captures: &Captures<'_>| {
            let word = captures.name("word").unwrap();
            if protected
                .iter()
                .any(|&(start, end)| start <= word.start() && word.start() < end)
                || word.as_str().chars().any(char::is_uppercase)
            {
                return captures[0].to_owned();
            }
            let first = word.as_str().chars().next().unwrap();
            let mut matched = captures[0].to_owned();
            let offset = word.start() - captures.get(0).unwrap().start();
            matched.replace_range(
                offset..offset + first.len_utf8(),
                &first.to_uppercase().collect::<String>(),
            );
            matched
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_whole_utterance_and_punctuation_does_not_double() {
        assert_eq!(whole_command("New line."), Some("\n"));
        assert_eq!(whole_command("a new line of research"), None);
        assert_eq!(whole_command("scratch that!"), Some(""));
        assert_eq!(
            spoken_punctuation("hello comma, world full stop. next question mark?"),
            "hello , world . next ?"
        );
        assert_eq!(
            capitalize_sentences("hello. world! iPhone stays. API stays.", &[]),
            "Hello. World! iPhone stays. API stays."
        );
    }
    #[test]
    fn bounded_restart_port_preserves_prose_and_negation() {
        for (text, expected) in [
            ("We need red. I mean, blue.", "Blue."),
            (
                "Send the report to. Send the report to Alice.",
                "Send the report to Alice.",
            ),
            (
                "It runs after the model. I'm sorry, it runs before the model.",
                "It runs before the model.",
            ),
            ("We need red. Scratch that. Blue.", "Blue."),
        ] {
            assert_eq!(crate::restarts::collapse_restarts(text), expected);
        }
        for text in [
            "I like it. It is good.",
            "That is the plan. That is what we agreed.",
            "No no, I had had enough.",
            "I do not want to cancel.",
            "Email Sarah. Sarah needs the file.",
            "We need the database. The database is down.",
            "Send it to Bob, I mean Alice.",
            "Hi John. I'm sorry I missed your call.",
            "Don't scratch that surface.",
        ] {
            assert_eq!(crate::restarts::collapse_restarts(text), text);
        }
    }
}
