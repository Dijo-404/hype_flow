use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use crate::config::Config;
use crate::vocabulary::{
    casefold, normalize_spaces, split_lines, terms, uppercase_first, whitespace,
};

static COMMON_WORDS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| include_str!("../assets/common_words.txt").lines().collect());
const LEGITIMATE_DOUBLES: &[&str] = &["had", "that", "very", "no", "yes", "ha", "so"];

pub const DEFAULT_FILTERS: &str = "um, uh, erm, umm, uhh";

// Python regex uses alphanumerics and underscore for \w, omitting combining marks.
const WORD: &str = r"[\p{L}\p{N}_]";
const EDGE: &str = r"[\p{L}\p{N}_'’\-]";
const SPACE: &str = r"[\p{White_Space}\x1c-\x1f]";
const BOUNDARY: &str =
    r"(?:(?<![\p{L}\p{N}_])(?=[\p{L}\p{N}_])|(?<=[\p{L}\p{N}_])(?![\p{L}\p{N}_]))";

static WORD_CHAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(WORD).unwrap());
static LETTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}").unwrap());
static ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]").unwrap());
static TITLECASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Lt}").unwrap());

static PROTECTED: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(&format!(
        r#"https?://[^{s}]+|www\.[^{s}]+|[\p{{L}}\p{{N}}_.+-]+@[\p{{L}}\p{{N}}_.-]+\.[A-Za-z]{{2,}}|`[^`\n]*`|"[^"\n]*"|(?<!{WORD})'[^'\n]*'(?!{WORD})"#,
        s = &SPACE[1..SPACE.len() - 1]
    ))
    .unwrap()
});

static REPEATED: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(&format!(r"{BOUNDARY}({EDGE}+)(?:{SPACE}+\1{BOUNDARY})+")).unwrap()
});
static HORIZONTAL_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static NOISE_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<unk>|\[(?:blank_audio|silence|noise|music|inaudible|applause)\]").unwrap()
});
static PUNCTUATION_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" +([,.;:!?])").unwrap());

pub(crate) fn corrections(value: &str) -> Result<Vec<(String, String)>> {
    ensure!(
        value.chars().count() <= 33_000,
        "Correction list is too long"
    );
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for (line_number, line) in split_lines(value).enumerate() {
        if line.trim_matches(whitespace).is_empty() {
            continue;
        }
        let number = line_number + 1;
        let message = || format!("Correction line {number}: use heard words => replacement");
        let (source, target) = line.split_once("=>").with_context(message)?;
        let source = normalize_spaces(source);
        let target = normalize_spaces(target);
        ensure!(
            !target.is_empty() && WORD_CHAR.is_match(&source),
            "{}",
            message()
        );
        ensure!(
            source.chars().count() <= 160
                && target.chars().count() <= 160
                && !line.bytes().any(|byte| byte < 32),
            "Correction line {number} is too long or contains control characters",
        );
        let key = casefold(&source);
        ensure!(
            seen.insert(key.clone()),
            "Duplicate correction on line {number}"
        );
        result.push((key, target));
    }
    ensure!(result.len() <= 100, "Use at most 100 correction rules");
    Ok(result)
}

fn pattern<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut values: Vec<_> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by_key(|value| std::cmp::Reverse(value.chars().count()));
    let choices: Vec<_> = values
        .iter()
        .map(|value| {
            let mut choice = String::new();
            for literal in value.chars() {
                match literal {
                    ' ' => {
                        choice.push_str(SPACE);
                        choice.push('+');
                    }
                    // Python's IGNORECASE puts all four forms of I in the same class.
                    'i' | 'I' | 'İ' | 'ı' => choice.push_str("[iIİı]"),
                    _ => choice.push_str(&regex::escape(literal.encode_utf8(&mut [0; 4]))),
                }
            }
            choice
        })
        .collect();
    Some(format!(r"(?<!{EDGE})(?i:{})(?!{EDGE})", choices.join("|")))
}

fn edit_matches(
    text: &str,
    pattern: &FancyRegex,
    mut replace: impl FnMut(&str, &mut String) -> Result<()>,
) -> Result<String> {
    let mut result = String::with_capacity(text.len());
    let mut end = 0;
    for matched in pattern.find_iter(text) {
        let matched = matched?;
        result.push_str(&text[end..matched.start()]);
        replace(matched.as_str(), &mut result)?;
        end = matched.end();
    }
    result.push_str(&text[end..]);
    Ok(result)
}

struct Replacements<'a> {
    pattern: FancyRegex,
    values: HashMap<&'a str, &'a str>,
}

impl<'a> Replacements<'a> {
    fn new(pairs: &'a [(String, String)]) -> Result<Option<Self>> {
        let Some(pattern) = pattern(pairs.iter().map(|(key, _)| key.as_str())) else {
            return Ok(None);
        };
        Ok(Some(Self {
            pattern: FancyRegex::new(&pattern)?,
            values: pairs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
        }))
    }

    fn apply(&self, text: &str) -> Result<String> {
        edit_matches(text, &self.pattern, |matched, output| {
            let key = casefold(&normalize_spaces(matched));
            let replacement = self
                .values
                .get(key.as_str())
                .with_context(|| format!("No correction for {matched:?} after case folding"))?;
            output.push_str(replacement);
            Ok(())
        })
    }
}

fn matches_char(pattern: &Regex, character: char) -> bool {
    pattern.is_match(character.encode_utf8(&mut [0; 4]))
}

fn alnum(character: char) -> bool {
    character.is_ascii_alphanumeric() || !character.is_ascii() && matches_char(&ALNUM, character)
}

fn key(word: &str) -> String {
    casefold(word)
        .chars()
        .filter(|&character| alnum(character))
        .collect()
}

fn soundex(word: &str) -> [u8; 4] {
    let code = |letter| match letter {
        b'b' | b'f' | b'p' | b'v' => b'1',
        b'c' | b'g' | b'j' | b'k' | b'q' | b's' | b'x' | b'z' => b'2',
        b'd' | b't' => b'3',
        b'l' => b'4',
        b'm' | b'n' => b'5',
        b'r' => b'6',
        _ => 0,
    };
    let mut bytes = word.bytes().map(|byte| byte.to_ascii_lowercase());
    let first = bytes.next().unwrap();
    let mut result = [first.to_ascii_uppercase(), b'0', b'0', b'0'];
    let mut previous = code(first);
    let mut length = 1;
    for letter in bytes {
        let current = code(letter);
        if current != 0 && current != previous {
            result[length] = current;
            length += 1;
            if length == result.len() {
                break;
            }
        }
        if !matches!(letter, b'h' | b'w') {
            previous = current;
        }
    }
    result
}

fn distance(first: &str, second: &str) -> usize {
    let mut row: Vec<_> = (0..=second.len()).collect();
    for (index, left) in first.bytes().enumerate() {
        let mut diagonal = row[0];
        row[0] = index + 1;
        for (column, right) in second.bytes().enumerate() {
            let above = row[column + 1];
            row[column + 1] = (above + 1)
                .min(row[column] + 1)
                .min(diagonal + usize::from(left != right));
            diagonal = above;
        }
    }
    row[second.len()]
}

struct FuzzyKey<'a> {
    key: String,
    word: &'a str,
    sound: Option<[u8; 4]>,
}

fn fuzzy_keys(vocabulary: &[(String, String)]) -> Vec<FuzzyKey<'_>> {
    let mut keys = Vec::new();
    for (_, word) in vocabulary {
        let first = key(word);
        let expanded = key(&word.replace('&', " and "));
        let alternate = (expanded != first).then_some(expanded);
        for value in [Some(first), alternate].into_iter().flatten() {
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                keys.push(FuzzyKey {
                    sound: value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphabetic())
                        .then(|| soundex(&value)),
                    key: value,
                    word,
                });
            }
        }
    }
    keys
}

fn consonants(value: &str) -> String {
    let mut result = String::new();
    for letter in value.chars().filter(|letter| !"aeiouhw".contains(*letter)) {
        if !result.ends_with(letter) {
            result.push(letter);
        }
    }
    result
}

fn closest<'a>(
    candidate: &str,
    keys: &'a [FuzzyKey<'a>],
    tolerance: f64,
) -> Option<(&'a str, f64)> {
    if candidate.is_empty() || candidate.len() > 50 || !candidate.is_ascii() {
        return None;
    }
    let exact: HashSet<_> = keys
        .iter()
        .filter(|key| key.key == candidate)
        .map(|key| key.word)
        .collect();
    if !exact.is_empty() {
        return (exact.len() == 1).then(|| (*exact.iter().next().unwrap(), 0.0));
    }
    if candidate.len() <= 5 || COMMON_WORDS.contains(candidate) {
        return None;
    }
    let sound = candidate
        .bytes()
        .all(|byte| byte.is_ascii_alphabetic())
        .then(|| soundex(candidate));
    let mut matches = HashMap::new();
    for key in keys {
        let longest = candidate.len().max(key.key.len());
        if key.key.len() <= 5
            || candidate.len().abs_diff(key.key.len()) as f64 > (longest as f64 * 0.25).max(2.0)
        {
            continue;
        }
        let score = distance(candidate, &key.key) as f64 / longest as f64;
        let same_sound = sound.is_some() && sound == key.sound;
        // Vowel-heavy names such as Shunade/Sinead share a consonant skeleton.
        let name_match =
            same_sound && consonants(candidate) == consonants(&key.key) && score <= 0.6;
        if score <= tolerance || same_sound && score <= 0.25 || name_match {
            matches
                .entry(key.word)
                .and_modify(|value: &mut f64| *value = value.min(score))
                .or_insert(score);
        }
    }
    (matches.len() == 1).then(|| matches.into_iter().next().unwrap())
}

fn edges(word: &str) -> (&str, &str) {
    let first = word.trim_start_matches(|character| !alnum(character));
    let last = first.trim_end_matches(|character| !alnum(character));
    (&word[..word.len() - first.len()], &first[last.len()..])
}

fn cased(original: &str, replacement: &str) -> String {
    let mut letters = original
        .chars()
        .filter(|&character| matches_char(&LETTER, character));
    if letters.next().is_some_and(char::is_uppercase) {
        if letters.clone().next().is_some() && letters.all(|character| character.is_uppercase()) {
            return replacement.to_uppercase();
        }
        return uppercase_first(replacement);
    }
    replacement.to_owned()
}

fn near_misses(
    text: &str,
    keys: &[FuzzyKey<'_>],
    tolerance: f64,
    allowed: Option<&HashSet<String>>,
) -> String {
    if keys.is_empty() {
        return text.to_owned();
    }
    let words: Vec<_> = text
        .split(whitespace)
        .filter(|word| !word.is_empty())
        .collect();
    let word_keys: Vec<_> = words.iter().map(|word| key(word)).collect();
    let word_edges: Vec<_> = words.iter().map(|word| edges(word)).collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < words.len() {
        let mut best = None;
        let mut best_score = f64::INFINITY;
        for size in (1..=3.min(words.len() - index)).rev() {
            let end = index + size;
            if word_edges[index..end - 1]
                .iter()
                .any(|(_, trailing)| !trailing.is_empty())
            {
                continue;
            }
            if words[index..end]
                .iter()
                .any(|word| word.contains(['@', '/', '\\']))
            {
                continue;
            }
            if allowed.is_some_and(|allowed| {
                word_keys[index..end]
                    .iter()
                    .any(|word| !allowed.contains(word))
            }) {
                continue;
            }
            let candidate = word_keys[index..end].concat();
            let exact = keys.iter().any(|key| key.key == candidate);
            let ordinary_phrase = size > 1
                && word_keys[index..end].iter().all(|word| word.len() > 1)
                && word_keys[index..end]
                    .iter()
                    .any(|word| COMMON_WORDS.contains(word.as_str()));
            if ordinary_phrase && !exact {
                continue;
            }
            if let Some((word, score)) = closest(&candidate, keys, tolerance)
                && score < best_score
            {
                best = Some((end, word));
                best_score = score;
            }
        }
        if index != 0 {
            result.push(' ');
        }
        if let Some((end, replacement)) = best {
            result.push_str(word_edges[index].0);
            result.push_str(&cased(words[index], replacement));
            result.push_str(word_edges[end - 1].1);
            index = end;
        } else {
            result.push_str(words[index]);
            index += 1;
        }
    }
    result
}

fn remove_repetitions(text: &str) -> Result<String> {
    // Python's case-insensitive backreferences compare simple lowercase characters.
    let mut lowered = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for (offset, character) in text.char_indices() {
        let lower = character.to_lowercase().next().unwrap();
        lowered.push(lower);
        offsets.extend(std::iter::repeat_n(offset, lower.len_utf8()));
    }
    offsets.push(text.len());
    let mut output = String::with_capacity(text.len());
    let mut end = 0;
    for captures in REPEATED.captures_iter(&lowered) {
        let captures = captures?;
        let repetition = captures.get(0).unwrap();
        let first_word = captures.get(1).unwrap();
        if LEGITIMATE_DOUBLES.contains(&first_word.as_str()) {
            continue;
        }
        output.push_str(&text[end..offsets[repetition.start()]]);
        output.push_str(&text[offsets[first_word.start()]..offsets[first_word.end()]]);
        end = offsets[repetition.end()];
    }
    output.push_str(&text[end..]);
    Ok(output)
}

fn vocabulary_echo(text: &str, vocabulary: &[(String, String)]) -> bool {
    let words: Vec<_> = text
        .split_whitespace()
        .map(key)
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        return false;
    }
    if vocabulary.iter().any(|(_, term)| {
        let term = key(term);
        !LEGITIMATE_DOUBLES.contains(&term.as_str())
            && words.iter().filter(|word| **word == term).count() >= 3
            && words.iter().filter(|word| **word == term).count() * 4 >= words.len() * 3
    }) {
        return true;
    }
    let mut cursor = 0;
    let mut matched = 0;
    let mut covered = 0;
    for (_, term) in vocabulary {
        let term_words: Vec<_> = term.split_whitespace().map(key).collect();
        if let Some(index) = words[cursor..]
            .windows(term_words.len())
            .position(|window| window == term_words)
        {
            cursor += index + term_words.len();
            covered += term_words.len();
            matched += 1;
        }
    }
    matched >= 3 && covered * 4 >= words.len() * 3
}

pub fn process(text: &str, values: &Config) -> Result<String> {
    process_with_confidence(text, values, &[])
}

/// Missing or ambiguous confidence never authorizes a guessed spelling change.
pub fn process_with_confidence(
    text: &str,
    values: &Config,
    words: &[crate::backend::WordConfidence],
) -> Result<String> {
    if values.spoken_commands
        && let Some(command) = crate::text_processing::whole_command(text)
    {
        return Ok(command.into());
    }
    let mut confidence = HashMap::<String, bool>::new();
    if values.confidence_filtering
        && text
            .split(whitespace)
            .map(key)
            .filter(|word| !word.is_empty())
            .eq(words
                .iter()
                .map(|word| key(&word.text))
                .filter(|word| !word.is_empty()))
    {
        for word in words {
            let low = word.min_log_probability.is_finite()
                && word.min_log_probability < values.confidence_threshold;
            confidence
                .entry(key(&word.text))
                .and_modify(|allowed| *allowed &= low)
                .or_insert(low);
        }
    }
    let allowed: HashSet<String> = confidence
        .into_iter()
        .filter_map(|(word, low)| low.then_some(word))
        .collect();
    let allowed = values.confidence_filtering.then_some(&allowed);
    let vocabulary: Vec<_> = terms(&values.prompt)?
        .into_iter()
        .map(|term| (casefold(&term), term))
        .collect();
    if !text.chars().any(alnum) || values.vocabulary_boosting && vocabulary_echo(text, &vocabulary)
    {
        return Ok(String::new());
    }
    let rules = corrections(&values.corrections)?;
    let blocked = terms(&values.blocked_words)?;
    let vocabulary_rules = values
        .auto_correct
        .then(|| Replacements::new(&vocabulary))
        .transpose()?
        .flatten();
    let replacements = values
        .auto_correct
        .then(|| Replacements::new(&rules))
        .transpose()?
        .flatten();
    let fuzzy = values.fuzzy_vocabulary.then(|| fuzzy_keys(&vocabulary));
    let filters = if values.filter_words
        && let Some(pattern) = pattern(blocked.iter().map(String::as_str))
    {
        Some(FancyRegex::new(&format!(
            r"(?:,{SPACE}*)?{pattern}(?:{SPACE}*[,;:])?"
        ))?)
    } else {
        None
    };
    let mut removed = false;
    let mut plain = |mut text: String| -> Result<String> {
        text = NOISE_TAGS.replace_all(&text, " ").into_owned();
        if values.remove_restarts {
            text = crate::restarts::collapse_restarts(&text);
        }
        // Resolve spoken punctuation before number grammars can consume its words.
        if values.spoken_commands {
            text = crate::text_processing::spoken_punctuation(&text);
        }
        if values.format_numbers {
            text = crate::itn::apply(&text);
        }
        if let Some(replacements) = &vocabulary_rules {
            text = replacements.apply(&text)?;
        }
        if let Some(replacements) = &replacements {
            text = replacements.apply(&text)?;
        }
        if let Some(keys) = &fuzzy
            && !keys.is_empty()
            && !text.trim_matches(whitespace).is_empty()
        {
            let trimmed_start = text.trim_start_matches(whitespace);
            let trimmed = trimmed_start.trim_end_matches(whitespace);
            let start = text.len() - trimmed_start.len();
            let matched = near_misses(trimmed, keys, values.fuzzy_tolerance, allowed);
            text.replace_range(start..start + trimmed.len(), &matched);
        }
        if values.spoken_commands {
            text = crate::text_processing::spoken_punctuation(&text);
        }
        if let Some(filters) = &filters {
            text = edit_matches(&text, filters, |matched, output| {
                let word = matched.trim_matches([' ', ',', ';', ':']);
                let mut uppercase = false;
                let preserve = word.chars().all(|character| {
                    uppercase |= character.is_uppercase();
                    !character.is_lowercase() && !matches_char(&TITLECASE, character)
                }) && uppercase
                    && word.chars().count() > 1
                    && !blocked.iter().any(|item| item == word);
                if preserve {
                    output.push_str(matched);
                } else {
                    removed = true;
                    output.push(' ');
                }
                Ok(())
            })?;
        }
        if values.remove_repetitions {
            text = remove_repetitions(&text)?;
        }
        let text = HORIZONTAL_SPACES.replace_all(&text, " ");
        Ok(PUNCTUATION_SPACES.replace_all(&text, "$1").into_owned())
    };

    let mut result = String::with_capacity(text.len());
    let mut end = 0;
    for protected in PROTECTED.find_iter(text) {
        let protected = protected?;
        result.push_str(&plain(text[end..protected.start()].to_owned())?);
        result.push_str(protected.as_str());
        end = protected.end();
    }
    result.push_str(&plain(text[end..].to_owned())?);
    let result = result.trim_matches(whitespace);
    let result = match result.strip_prefix([',', ';', ':']) {
        Some(rest) => rest.trim_start_matches(whitespace),
        None => result,
    };
    if !result.chars().any(alnum) || removed && !WORD_CHAR.is_match(result) {
        return Ok(String::new());
    }
    let result = result.to_owned();
    Ok(
        if values.capitalize_after_filler && removed || values.spoken_commands {
            crate::text_processing::capitalize_sentences(
                &result,
                &PROTECTED
                    .find_iter(&result)
                    .filter_map(|matched| {
                        matched.ok().map(|matched| (matched.start(), matched.end()))
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            result
        },
    )
}

#[cfg(test)]
mod product_tests {
    use super::*;
    #[test]
    fn optional_text_features_keep_protected_content_and_prose() {
        let values = Config {
            spoken_commands: true,
            format_numbers: true,
            remove_restarts: true,
            capitalize_after_filler: true,
            ..Config::default()
        };
        assert_eq!(process("new line.", &values).unwrap(), "\n");
        assert_eq!(process("comma.", &values).unwrap(), ",");
        assert_eq!(process("question mark.", &values).unwrap(), "?");
        for literal in [
            "`lower.case`",
            "\"don't change me.\"",
            "https://example.com",
        ] {
            assert_eq!(
                process(&format!("um, {literal}"), &values).unwrap(),
                literal
            );
        }
        assert_eq!(
            process("um, twenty five dollars comma, please", &values).unwrap(),
            "$25, please"
        );
        assert_eq!(
            process(
                "um, send it to `twenty five dollars` via https://example.com/comma",
                &values
            )
            .unwrap(),
            "Send it to `twenty five dollars` via https://example.com/comma"
        );
        assert_eq!(
            process("One of the things. No one knows.", &values).unwrap(),
            "One of the things. No one knows."
        );
    }
    #[test]
    fn confidence_only_permits_low_confidence_unambiguous_words() {
        let values = Config {
            prompt: "Sinead".into(),
            fuzzy_vocabulary: true,
            confidence_filtering: true,
            ..Config::default()
        };
        let mut word = crate::backend::WordConfidence {
            text: "Shunade".into(),
            start_seconds: 0.0,
            end_seconds: 0.5,
            min_log_probability: -0.1,
            duration_is_reliable: false,
        };
        assert_eq!(
            process_with_confidence("Shunade", &values, &[word.clone()]).unwrap(),
            "Shunade"
        );
        word.min_log_probability = -2.0;
        assert_eq!(
            process_with_confidence("Shunade Shunade", &values, std::slice::from_ref(&word))
                .unwrap(),
            "Shunade Shunade"
        );
        assert_eq!(
            process_with_confidence("Shunade", &values, &[word]).unwrap(),
            "Sinead"
        );
        assert_eq!(process("Shunade", &values).unwrap(), "Shunade");
    }
}
