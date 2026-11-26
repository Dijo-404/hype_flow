// Adapted from Voxtype (MIT), copyright 2025 Peter Jackson.
// Original license: ../docs/licenses/voxtype.txt.
// Inverse text normalization for dictated numbers, money and times.
//
// Turns spoken forms into written ones: "twenty five dollars" -> "$25",
// "three thirty p m" -> "3:30 p.m.". The conversion itself comes from
// `text-processing-rs`, a Rust port of NeMo's WFST grammars.
//
// # Why this module exists rather than calling the crate directly
//
// The crate is tuned for numeric utterances, and applied to free prose it
// does real damage, because English number words are also ordinary words:
//
// ```text
// one of the things we need  ->  1 of the things we need
// no one knows               ->  no 1 knows
// the first thing            ->  the 1st thing
// send it to pete at ...     ->  sendittopete@...   (spaces eaten)
// ```
//
// Dictation is mostly prose, so this module only lets the converter near
// text that looks like it is actually about a quantity, and then rejects any
// individual rewrite that turns a lone number word into a digit. What is
// left converts the cases worth converting and leaves prose alone.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Words that make a sentence plausibly about a quantity.
const CURRENCY: &[&str] = &[
    "dollar", "dollars", "cent", "cents", "euro", "euros", "pound", "pounds", "pence", "yen",
    "percent",
];

/// Time-of-day markers.
const TIME_MARKERS: &[&str] = &["am", "pm", "a.m.", "p.m.", "o'clock", "oclock"];

/// Units common enough in dictation to be worth converting alongside.
const UNITS: &[&str] = &[
    "minute",
    "minutes",
    "hour",
    "hours",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "year",
    "years",
    "megabyte",
    "megabytes",
    "gigabyte",
    "gigabytes",
    "kilometre",
    "kilometres",
    "kilometer",
    "kilometers",
    "mile",
    "miles",
    "degree",
    "degrees",
];

/// Number words, used to spot a compound like "twenty five".
const NUMBER_WORDS: &[&str] = &[
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "thousand",
    "million",
    "billion",
];

/// Number words that multiply what precedes them.
const SCALES: &[&str] = &["hundred", "thousand", "million", "billion"];

/// Single words that must never become digits on their own. These are the
/// ones that read as prose far more often than as quantities.
const NEVER_ALONE: &[&str] = &[
    "one", "two", "three", "first", "second", "third", "fourth", "fifth", "sixth", "seventh",
    "eighth", "ninth", "tenth", "no", "a", "an",
];

fn set_of(words: &'static [&'static str]) -> HashSet<&'static str> {
    words.iter().copied().collect()
}

pub(crate) fn number_words() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| set_of(NUMBER_WORDS))
}

fn never_alone() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| set_of(NEVER_ALONE))
}

fn normalize_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '\'')
        .to_lowercase()
}

/// Whether a sentence is worth handing to the converter at all.
fn has_quantity_trigger(sentence: &str) -> bool {
    let words: Vec<String> = sentence.split_whitespace().map(normalize_word).collect();
    // The last word keeps the sentence's full stop ("dollars."); "a.m." needs it.
    let bare: Vec<&str> = words.iter().map(|w| w.trim_end_matches('.')).collect();
    let is_number =
        |w: &str| number_words().contains(w) || w.starts_with(|c: char| c.is_ascii_digit());

    let marker = (0..words.len()).any(|i| {
        let w = bare[i];
        let time = TIME_MARKERS.contains(&words[i].as_str()) || TIME_MARKERS.contains(&w);
        // "I am sending" is a verb: a bare am/pm only counts after a number.
        let time = time && (!matches!(w, "am" | "pm") || i > 0 && is_number(bare[i - 1]));
        CURRENCY.contains(&w) || UNITS.contains(&w) || time
    });
    if marker {
        return true;
    }
    // "a m" / "p m" arrive as two tokens from most engines.
    for pair in bare.windows(2) {
        if (pair[0] == "a" || pair[0] == "p") && pair[1] == "m" {
            return true;
        }
    }
    // A compound number ("twenty five") is a quantity almost by definition.
    bare.windows(2)
        .any(|p| number_words().contains(p[0]) && number_words().contains(p[1]))
}

/// The value of a number word below a hundred.
fn small_value(w: &str) -> Option<u64> {
    match NUMBER_WORDS.iter().position(|n| *n == w)? as u64 {
        i @ 0..=19 => Some(i),
        i @ 20..=27 => Some((i - 18) * 10),
        _ => None,
    }
}

/// The value of number words read as one number ("two hundred and five"),
/// or None when they read as several ("twenty twenty", "five ten").
fn cardinal(run: &[String]) -> Option<u64> {
    let (mut total, mut group, mut last_scale) = (0u64, 0u64, u64::MAX);
    for w in run {
        let scale = match w.as_str() {
            "and" => continue,
            "hundred" if group < 100 => {
                group = group.max(1) * 100;
                continue;
            }
            "thousand" => 1_000,
            "million" => 1_000_000,
            "billion" => 1_000_000_000,
            w => {
                let v = small_value(w)?;
                let low = group % 100;
                // Only a tens word takes another word below a hundred: "twenty five".
                if low != 0 && !(low >= 20 && low % 10 == 0 && (1..10).contains(&v)) {
                    return None;
                }
                group += v;
                continue;
            }
        };
        if scale >= last_scale {
            return None;
        }
        total += group.max(1) * scale;
        group = 0;
        last_scale = scale;
    }
    Some(total + group)
}

/// Years, clock times and digit strings are said in parts: "twenty twenty",
/// "three thirty", "nine one one".
fn parts(run: &[String]) -> Option<Vec<u64>> {
    let mut parts: Vec<u64> = Vec::new();
    for w in run {
        let v = small_value(w)?;
        match parts.last_mut() {
            Some(last) if *last >= 20 && *last % 10 == 0 && (1..10).contains(&v) => *last += v,
            _ => parts.push(v),
        }
    }
    Some(parts)
}

/// Runs of number words in a rewrite's source, hyphens split.
fn number_runs(words: &[&str]) -> Vec<Vec<String>> {
    let words: Vec<String> = words
        .iter()
        .flat_map(|w| w.split('-'))
        .map(|w| normalize_word(w).trim_end_matches('.').to_string())
        .collect();
    let in_run = |i: usize| {
        let w = words[i].as_str();
        number_words().contains(w)
            || w == "and"
                && i > 0
                && SCALES.contains(&words[i - 1].as_str())
                && words
                    .get(i + 1)
                    .is_some_and(|n| number_words().contains(n.as_str()))
    };
    let mut runs = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let start = i;
        while i < words.len() && in_run(i) {
            i += 1;
        }
        // A decimal's digits after "point" are said one by one.
        if i > start && (start == 0 || words[start - 1] != "point") {
            runs.push(words[start..i].to_vec());
        }
        i = i.max(start + 1);
    }
    runs
}

/// Reject a rewrite that turns a single prose-heavy word into a digit.
fn rewrite_is_safe(before: &[&str], after: &[&str]) -> bool {
    // Eating whitespace across several words is the email/URL failure mode.
    if before.len() > 1 && after.len() == 1 && after[0].contains('@') {
        return false;
    }
    if before.len() == 1 {
        let w = normalize_word(before[0]);
        if never_alone().contains(w.as_str()) {
            return false;
        }
    }
    // The converter adds or glues numbers said apart: "twenty twenty
    // dollars" -> "$40", "five ten minutes" -> "510 minutes".
    let digits: String = after
        .concat()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let clock = after.iter().any(|w| w.contains(':'));
    number_runs(before)
        .iter()
        .filter(|run| run.len() > 1)
        .all(|run| {
            cardinal(run).is_some_and(|v| digits.contains(&v.to_string()))
                || parts(run).is_some_and(|p| {
                    let said_in_parts = p.len() == 2 && (clock || p.iter().all(|v| *v >= 10))
                        || p.len() >= 3 && p.iter().all(|v| *v < 10);
                    said_in_parts
                        && digits.contains(&p.iter().map(u64::to_string).collect::<String>())
                })
        })
}

/// Marks original words the converter must leave alone: "to", "or" or "and"
/// after a number, plus the numbers on either side. It reads "two to three"
/// as a time, "five and ten" as one number, and drops the "and" of "thirty
/// five and she".
fn joined_numbers(words: &[&str]) -> Vec<bool> {
    let words: Vec<String> = words
        .iter()
        .map(|w| normalize_word(w).trim_end_matches('.').to_string())
        .collect();
    let number =
        |w: &str| number_words().contains(w) || w.starts_with(|c: char| c.is_ascii_digit());
    let mut keep = vec![false; words.len()];
    for k in 1..words.len() {
        let prev = words[k - 1].rsplit('-').next().unwrap_or_default();
        let conj = match words[k].as_str() {
            "to" | "or" => true,
            // "a hundred and twenty" is one number.
            "and" => !SCALES.contains(&prev),
            _ => false,
        };
        if !conj || !number(prev) {
            continue;
        }
        keep[k] = true;
        if words
            .get(k + 1)
            .is_some_and(|n| number(n.split('-').next().unwrap_or_default()))
        {
            keep[k - 1] = true;
            keep[k + 1] = true;
        }
    }
    keep
}

/// Split on sentence terminators, keeping the terminator with its sentence.
///
/// A full stop is not automatically a sentence end. Splitting naively cut
/// "Meet me at 3.30 p.m." into three fragments, none of which still looked
/// like a time, so nothing was converted. A terminator only ends a sentence
/// when it is not inside a decimal, not part of a single-letter abbreviation
/// ("p.m.", "e.g."), and is followed by whitespace then a capital - or by
/// nothing at all.
fn sentences(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0usize;

    for (idx, &(_, c)) in chars.iter().enumerate() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let prev = idx.checked_sub(1).map(|k| chars[k].1);
        let next = chars.get(idx + 1).map(|&(_, c)| c);

        if c == '.' {
            // A decimal point: "3.30".
            if prev.is_some_and(|p| p.is_ascii_digit()) && next.is_some_and(|n| n.is_ascii_digit())
            {
                continue;
            }
            // A single-letter abbreviation: the "p." and "m." of "p.m.".
            let two_back = idx.checked_sub(2).map(|k| chars[k].1);
            if prev.is_some_and(|p| p.is_alphabetic())
                && two_back.is_none_or(|b| !b.is_alphanumeric())
            {
                continue;
            }
        }

        // Only a capital (or the end of the text) starts a new sentence.
        let mut j = idx + 1;
        while let Some(&(_, w)) = chars.get(j) {
            if w.is_whitespace() {
                j += 1;
            } else {
                break;
            }
        }
        match chars.get(j) {
            Some(&(_, w)) if !w.is_uppercase() => continue,
            _ => {}
        }

        let cut = chars.get(j).map(|&(k, _)| k).unwrap_or(text.len());
        out.push(&text[start..cut]);
        start = cut;
    }

    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Split hyphenated number compounds so the converter can see them:
/// "Twenty-five" -> "Twenty five". Only fires when both halves are number
/// words, so "well-known" and "twenty-odd" are untouched.
fn despace_hyphenated_numbers(sentence: &str) -> String {
    sentence
        .split_whitespace()
        .map(|word| {
            let core = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
            if let Some((a, b)) = core.split_once('-')
                && number_words().contains(a.to_lowercase().as_str())
                && number_words().contains(b.to_lowercase().as_str())
            {
                return word.replacen('-', " ", 1);
            }
            word.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The converter emits a zero-padded hour ("03:30 p.m."), which is wrong for
/// a 12-hour clock.
fn strip_leading_zero_hour(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b0(\d:\d{2})")
            .expect("BUG: leading-zero regex is a compile-time constant and must be valid")
    });
    re.replace_all(text, "$1").into_owned()
}

/// Engines write a clock time with a full stop ("3.30 p.m."). Only rewrite
/// when a meridiem marker follows, so a decimal or a price is never touched.
fn fix_time_separator(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(\d{1,2})\.(\d{2})(\s*[ap]\.?m\b\.?)")
            .expect("BUG: time-separator regex is a compile-time constant and must be valid")
    });
    re.replace_all(text, "$1:$2$3").into_owned()
}

/// Apply inverse text normalization, conservatively.
pub fn apply(text: &str) -> String {
    let opts = text_processing_rs::NormalizeOptions {
        concat_compound_numbers: true,
        max_span_tokens: None,
        // "give me a second" must not become "a 2nd".
        disable_bare_second: true,
    };

    let mut out = String::with_capacity(text.len());
    for sentence in sentences(text) {
        if !has_quantity_trigger(sentence) {
            out.push_str(sentence);
            continue;
        }
        // Engines hyphenate compounds ("Twenty-five dollars"), which the
        // converter's grammars do not recognise. Feed it spaced words but
        // reconcile against what the user actually said.
        let spaced = despace_hyphenated_numbers(sentence);
        let converted = text_processing_rs::normalize_sentence_with_options(&spaced, opts);
        let converted = strip_leading_zero_hour(&converted);
        let converted = fix_time_separator(&converted);
        // A sentence-final "pm." becomes "p.m." plus the sentence's own full stop.
        let converted = converted.replace("a.m..", "a.m.").replace("p.m..", "p.m.");
        out.push_str(&reconcile(sentence, &converted));
    }
    out
}

/// Accept the converter's rewrites one at a time, keeping the original words
/// wherever a rewrite looks like prose damage.
fn reconcile(original: &str, converted: &str) -> String {
    use crate::text_diff::{SpanKind, word_diff};

    let spans = word_diff(original, converted);
    let joined = joined_numbers(&original.split_whitespace().collect::<Vec<_>>());
    // Position in the original's words, which Same and Deleted spans cover.
    let mut at = 0usize;
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < spans.len() {
        match spans[i].kind {
            SpanKind::Same => {
                at += spans[i].text.split_whitespace().count();
                parts.push(spans[i].text.clone());
            }
            SpanKind::Deleted => {
                // A deletion followed by an insertion is one replacement.
                let inserted = spans
                    .get(i + 1)
                    .filter(|s| s.kind == SpanKind::Inserted)
                    .map(|s| s.text.clone());
                let before: Vec<&str> = spans[i].text.split_whitespace().collect();
                let touches_joined = joined[at..at + before.len()].contains(&true);
                at += before.len();
                match inserted {
                    Some(after_text) => {
                        let after: Vec<&str> = after_text.split_whitespace().collect();
                        if !touches_joined && rewrite_is_safe(&before, &after) {
                            parts.push(after_text);
                        } else {
                            parts.push(spans[i].text.clone());
                        }
                        i += 1; // consume the insertion too
                    }
                    // Pure deletion: the converter dropped words. Never
                    // silently lose dictation, so keep them.
                    None => parts.push(spans[i].text.clone()),
                }
            }
            SpanKind::Inserted => parts.push(spans[i].text.clone()),
        }
        i += 1;
    }

    // Preserve the original's trailing whitespace so sentences rejoin cleanly.
    let trailing: String = original
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .collect();
    format!("{}{}", parts.join(" "), trailing)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Parakeet output, dictated by the maintainer. These are the cases
    /// that must not regress: the engine already normalises some numbers and
    /// hyphenates compounds, so the input here is not raw spoken form.
    #[test]
    fn real_engine_output_prose_is_untouched() {
        for s in [
            "22 open issues.",
            "One of the things we need.",
            "No one knows.",
            "The first thing we need.",
            "Send it to Pete at turn.travel.",
            "Version one point two point three",
        ] {
            assert_eq!(apply(s), s, "must not rewrite: {s}");
        }
    }

    #[test]
    fn hyphenated_currency_converts() {
        assert_eq!(apply("Twenty-five dollars a month."), "$25 a month.");
    }

    #[test]
    fn spoken_form_currency_and_time_convert() {
        assert_eq!(
            apply("it will cost twenty five dollars a month"),
            "it will cost $25 a month"
        );
        assert_eq!(apply("meet me at three thirty p m"), "meet me at 3:30 p.m.");
    }

    #[test]
    fn a_lone_number_word_never_becomes_a_digit() {
        // Even inside a sentence the converter is allowed to touch, a bare
        // prose-heavy number word is kept.
        assert_eq!(
            apply("one of the things costs twenty five dollars"),
            "one of the things costs $25"
        );
    }

    #[test]
    fn bare_second_stays_a_second() {
        assert_eq!(apply("give me a second"), "give me a second");
        assert_eq!(
            apply("give me a second, it costs twenty five dollars"),
            "give me a second, it costs $25"
        );
    }

    #[test]
    fn sentences_without_a_quantity_are_skipped_entirely() {
        let s = "We shipped the cleanup pipeline and it works well.";
        assert_eq!(apply(s), s);
    }

    #[test]
    fn multi_sentence_text_keeps_its_shape() {
        let out = apply("No one knows. It costs twenty five dollars. The first thing we need.");
        assert_eq!(out, "No one knows. It costs $25. The first thing we need.");
    }

    #[test]
    fn hyphen_splitting_only_applies_to_number_pairs() {
        let s = "It is a well-known follow-up issue.";
        assert_eq!(apply(s), s);
    }

    #[test]
    fn sentence_final_quantities_convert_but_the_verb_am_does_not() {
        assert_eq!(apply("It costs five dollars."), "It costs $5.");
        assert_eq!(
            apply("I'll be there in ten minutes."),
            "I'll be there in 10 minutes."
        );
        assert_eq!(apply("Meet at five p m."), "Meet at 5:00 p.m.");
        assert_eq!(apply("Meet at five pm."), "Meet at 5:00 p.m.");
        for s in [
            "I am sending four files to nine people",
            "I am here at five.",
        ] {
            assert_eq!(apply(s), s);
        }
    }

    #[test]
    fn numbers_said_apart_are_not_merged() {
        for s in [
            "It took two to three hours.",
            "It costs between five and ten dollars.",
            "It costs five or ten dollars.",
            "Twenty-five to thirty dollars.",
            "it was five ten minutes",
            "I have two three year olds",
            "It was twenty twenty dollars.",
            "It costs one thousand two hundred dollars.",
        ] {
            assert_eq!(apply(s), s, "must not merge: {s}");
        }
        assert_eq!(
            apply("He is thirty five and she is forty years."),
            "He is thirty five and she is 40 years."
        );
        for (s, expected) in [
            ("a hundred and twenty days", "120 days"),
            (
                "one thousand two hundred people waited",
                "1200 people waited",
            ),
            ("twenty five dollars and fifty cents", "$25.50"),
            ("twenty five hundred dollars", "$2500"),
            ("twenty twenty one was a year", "2021 was a year"),
            ("pi is three point one four", "pi is 3.14"),
            ("call five five five zero one two three", "call 5550123"),
            ("eleven fifty nine p m", "11:59 p.m."),
        ] {
            assert_eq!(apply(s), expected);
        }
    }

    #[test]
    fn engine_clock_times_get_a_colon() {
        assert_eq!(apply("Meet me at 3.30 p.m."), "Meet me at 3:30 p.m.");
        assert_eq!(apply("Standup is 4.15 a.m."), "Standup is 4:15 a.m.");
    }

    #[test]
    fn decimals_and_prices_keep_their_full_stop() {
        // No meridiem marker, so these must not become clock times.
        assert_eq!(apply("it costs $3.30 a month"), "it costs $3.30 a month");
        assert_eq!(apply("version 1.20 shipped"), "version 1.20 shipped");
        assert_eq!(
            apply("I paid 3.30 amazingly fast in five days"),
            "I paid 3.30 amazingly fast in 5 days"
        );
    }
}
