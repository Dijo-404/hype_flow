use std::collections::HashSet;
use std::{fs, path::Path};

use anyhow::{Context, Result, bail, ensure};
use unicode_casefold::UnicodeCaseFold;

pub fn whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\x1c'..='\x1f')
}

pub(crate) fn normalize_spaces(text: &str) -> String {
    text.split(whitespace)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn casefold(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        let mut folded = character.case_fold();
        let first = folded.next().unwrap();
        // The crate uses Unicode 9. Newer mappings lowercase, except Cherokee folds to uppercase.
        if first == character
            && folded.clone().next().is_none()
            && !matches!(character, '\u{13a0}'..='\u{13f5}')
        {
            result.extend(character.to_lowercase());
        } else {
            result.push(first);
            result.extend(folded);
        }
    }
    result
}

pub(crate) fn split_lines(mut text: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        let next = text.find([
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ]);
        match next {
            Some(end) => {
                let (line, tail) = text.split_at(end);
                let length = if tail.starts_with("\r\n") {
                    2
                } else {
                    tail.chars().next().unwrap().len_utf8()
                };
                text = &tail[length..];
                Some(line)
            }
            None => Some(std::mem::take(&mut text)),
        }
    })
}

pub fn terms(value: &str) -> Result<Vec<String>> {
    ensure!(value.chars().count() <= 20_000, "Word list is too long");
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for item in value.split([',', '\n', '\r']) {
        let item = normalize_spaces(item);
        if item.is_empty() || !seen.insert(casefold(&item)) {
            continue;
        }
        ensure!(
            item.chars().count() <= 80 && !item.bytes().any(|byte| byte < 32),
            "Each word or phrase must be at most 80 characters without control characters",
        );
        result.push(item);
    }
    ensure!(
        result.len() <= 100,
        "Use at most 100 words or phrases per list"
    );
    Ok(result)
}

pub(crate) fn uppercase_first(text: &str) -> String {
    match text.chars().next() {
        Some(first) => first
            .to_uppercase()
            .chain(text[first.len_utf8()..].chars())
            .collect(),
        None => String::new(),
    }
}

pub fn hotwords(value: &str) -> Result<String> {
    let mut result = Vec::new();
    for term in terms(value)? {
        ensure!(
            !term.split(' ').any(|word| word.starts_with(':')),
            "Vocabulary words cannot begin with a colon"
        );
        let lowercase = term.to_lowercase();
        let uppercase = uppercase_first(&term);
        for variant in [term, lowercase, uppercase] {
            if result.len() < 100 && !result.contains(&variant) {
                result.push(variant);
            }
        }
    }
    Ok(result.join("\n"))
}

fn varint(data: &mut &[u8]) -> Result<u64> {
    let mut result = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = data.split_first().context("Truncated tokenizer integer")?;
        *data = rest;
        ensure!(shift < 63 || byte <= 1, "Invalid tokenizer integer");
        result |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(result);
        }
    }
    bail!("Invalid tokenizer integer")
}

fn field<'a>(data: &mut &'a [u8]) -> Result<(u64, u8, &'a [u8])> {
    let key = varint(data)?;
    let number = key >> 3;
    let wire = (key & 7) as u8;
    ensure!(number != 0, "Invalid tokenizer field");
    let length = match wire {
        0 => {
            varint(data)?;
            0
        }
        1 => 8,
        2 => usize::try_from(varint(data)?)?,
        5 => 4,
        _ => bail!("Unsupported tokenizer field"),
    };
    let (value, rest) = data
        .split_at_checked(length)
        .context("Truncated tokenizer field")?;
    *data = rest;
    Ok((number, wire, value))
}

pub fn sentencepiece_vocabulary(mut data: &[u8], tokens: &Path) -> Result<String> {
    ensure!(
        !data.is_empty() && data.len() <= 1_000_000,
        "Unexpected tokenizer size"
    );
    let mut pieces = HashSet::new();
    let mut output = String::new();
    while !data.is_empty() {
        let (number, wire, mut message) = field(&mut data)?;
        if (number, wire) != (1, 2) {
            continue;
        }
        let (mut piece, mut score) = (None, None);
        while !message.is_empty() {
            match field(&mut message)? {
                (1, 2, value) => piece = Some(value),
                (2, 5, value) => score = Some(f32::from_le_bytes(value.try_into()?)),
                _ => {}
            }
        }
        let piece = str::from_utf8(piece.context("Invalid tokenizer piece")?)
            .context("Invalid tokenizer piece")?;
        let score = score.context("Invalid tokenizer piece")?;
        ensure!(
            !piece.is_empty() && !piece.chars().any(whitespace) && score.is_finite(),
            "Invalid tokenizer piece or score",
        );
        ensure!(pieces.insert(piece), "Duplicate tokenizer piece");
        output.push_str(piece);
        output.push('\t');
        output.push_str(&format_score(score));
        output.push('\n');
    }

    let token_list = fs::read_to_string(tokens)?;
    let expected: HashSet<_> = split_lines(&token_list)
        .map(|line| line.rsplit_once(' ').map_or(line, |(token, _)| token))
        .filter(|&token| token != "<blk>")
        .collect();
    ensure!(
        pieces == expected,
        "Tokenizer vocabulary does not match the installed speech model"
    );
    Ok(output)
}

fn format_score(score: f32) -> String {
    // SentencePiece exports nine significant digits using Python's ".9g".
    let scientific = format!("{score:.8e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap();
    let exponent: i32 = exponent.parse().unwrap();
    if (-4..9).contains(&exponent) {
        let places = (8 - exponent) as usize;
        let decimal = format!("{score:.places$}");
        return if places == 0 {
            decimal
        } else {
            decimal
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        };
    }
    let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
    format!("{mantissa}e{exponent:+03}")
}

#[cfg(test)]
mod hotword_tests {
    use super::*;

    #[test]
    fn native_variants_stay_within_one_hundred_entries() {
        let list = (0..100)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let result = hotwords(&list).unwrap();
        assert_eq!(result.lines().count(), 100);
        assert_eq!(result.lines().collect::<HashSet<_>>().len(), 100);
        let invalid_tail = (0..99)
            .map(|i| format!("word{i}"))
            .chain(["bad :99".into()])
            .collect::<Vec<_>>()
            .join(",");
        assert!(hotwords(&invalid_tail).is_err());
    }
}
