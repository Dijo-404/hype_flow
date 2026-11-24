use anyhow::{Context, Result, ensure};
use hype_flow::vocabulary::{casefold, whitespace};
use regex::Regex;
use serde::Serialize;
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::LazyLock,
};
use unicode_normalization::UnicodeNormalization;

static PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{P}").unwrap());

pub fn words(text: &str) -> Vec<String> {
    let normalized = casefold(&text.nfkc().collect::<String>());
    PUNCTUATION
        .replace_all(&normalized, " ")
        .split(whitespace)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn formatted(text: &str) -> Vec<String> {
    PUNCTUATION
        .replace_all(&text.nfkc().collect::<String>(), " $0 ")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

// Keep the published normalizer exact rather than approximating its number grammar.
// This evaluation-only subprocess uses Python's standard library; dictation stays native.
pub fn whisper(texts: &[&str]) -> Result<Vec<Vec<String>>> {
    let basic = include_str!("../../../tools/normalizers/basic.py");
    let english = include_str!("../../../tools/normalizers/english.py")
        .replace("from .basic import remove_symbols_and_diacritics", "");
    let mapping = serde_json::to_string(include_str!("../../../tools/normalizers/english.json"))?;
    let script = format!(
        "import json, sys\n{basic}\n{english}\nnormalizer = EnglishTextNormalizer(json.loads({mapping}))\nprint(json.dumps([normalizer(s).split() for s in json.load(sys.stdin)]))"
    );
    let mut child = Command::new("python3")
        .args(["-I", "-S", "-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Whisper scoring requires python3 (standard library only)")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(texts)?)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "Whisper normalizer: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("Invalid Whisper normalization output")
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct Errors {
    pub substitutions: usize,
    pub insertions: usize,
    pub deletions: usize,
}
impl Errors {
    pub fn total(self) -> usize {
        self.substitutions + self.insertions + self.deletions
    }
}

pub fn edits(reference: &[String], hypothesis: &[String]) -> Errors {
    let width = hypothesis.len() + 1;
    let mut matrix = vec![0usize; (reference.len() + 1) * width];
    for (col, cell) in matrix[..width].iter_mut().enumerate() {
        *cell = col;
    }
    for row in 1..=reference.len() {
        matrix[row * width] = row;
        for col in 1..width {
            matrix[row * width + col] = (matrix[(row - 1) * width + col] + 1)
                .min(matrix[row * width + col - 1] + 1)
                .min(
                    matrix[(row - 1) * width + col - 1]
                        + usize::from(reference[row - 1] != hypothesis[col - 1]),
                );
        }
    }
    let (mut row, mut col) = (reference.len(), hypothesis.len());
    let mut counts = Errors::default();
    while row > 0 || col > 0 {
        if row > 0
            && col > 0
            && matrix[row * width + col]
                == matrix[(row - 1) * width + col - 1]
                    + usize::from(reference[row - 1] != hypothesis[col - 1])
        {
            counts.substitutions += usize::from(reference[row - 1] != hypothesis[col - 1]);
            row -= 1;
            col -= 1;
        } else if row > 0 && matrix[row * width + col] == matrix[(row - 1) * width + col] + 1 {
            counts.deletions += 1;
            row -= 1;
        } else {
            counts.insertions += 1;
            col -= 1;
        }
    }
    counts
}

#[derive(Default, Serialize)]
pub struct Score {
    pub reference_words: usize,
    pub errors: usize,
    pub substitutions: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub word_error_rate: f64,
}
impl Score {
    pub fn add(&mut self, reference: &[String], hypothesis: &[String]) -> Errors {
        let e = edits(reference, hypothesis);
        self.reference_words += reference.len();
        self.errors += e.total();
        self.substitutions += e.substitutions;
        self.insertions += e.insertions;
        self.deletions += e.deletions;
        self.word_error_rate = self.errors as f64 / self.reference_words.max(1) as f64;
        e
    }
}

#[derive(Default, Serialize)]
pub struct Keywords {
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}
impl Keywords {
    pub fn add(&mut self, terms: &[Vec<String>], reference: &[String], hypothesis: &[String]) {
        for term in terms.iter().filter(|t| !t.is_empty()) {
            let count = |text: &[String]| {
                text.windows(term.len())
                    .filter(|part| *part == term)
                    .count()
            };
            let (expected, actual) = (count(reference), count(hypothesis));
            self.true_positives += expected.min(actual);
            self.false_positives += actual.saturating_sub(expected);
            self.false_negatives += expected.saturating_sub(actual);
        }
        self.precision =
            self.true_positives as f64 / (self.true_positives + self.false_positives).max(1) as f64;
        self.recall =
            self.true_positives as f64 / (self.true_positives + self.false_negatives).max(1) as f64;
        self.f1 = if self.precision + self.recall == 0.0 {
            0.0
        } else {
            2.0 * self.precision * self.recall / (self.precision + self.recall)
        };
    }
}

pub fn bucket(seconds: f64) -> &'static str {
    if seconds < 5.0 {
        "under_5s"
    } else if seconds <= 20.0 {
        "5_to_20s"
    } else if seconds <= 60.0 {
        "20_to_60s"
    } else {
        "over_60s"
    }
}
pub fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((quantile * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_breakdown_and_unicode() {
        assert_eq!(words("Straße, ＡＢＣ! नमस्ते"), ["strasse", "abc", "नमस्ते"]);
        assert_eq!(
            edits(&words("a b c"), &words("a x c d")),
            Errors {
                substitutions: 1,
                insertions: 1,
                deletions: 0
            }
        );
        assert_eq!(edits(&words("a"), &[]).deletions, 1);
        assert_eq!(edits(&[], &words("a")).insertions, 1);
        assert_eq!(formatted("Hello, world!"), ["Hello", ",", "world", "!"]);
    }
    #[test]
    fn keyword_boundaries_and_repeated_insertions() {
        let mut k = Keywords::default();
        k.add(
            &[words("Rust"), words("Open AI")],
            &words("Rust Open AI"),
            &words("trust Rust Rust"),
        );
        assert_eq!(
            (k.true_positives, k.false_positives, k.false_negatives),
            (1, 1, 1)
        );
        assert_eq!(k.f1, 0.5);
    }
    #[test]
    fn standard_whisper_normalizer() {
        let texts = whisper(&[
            "Um, I can't colour twenty five dollars.",
            "I can not color $25",
        ])
        .unwrap();
        assert_eq!(texts[0], texts[1]);
    }
    #[test]
    fn buckets_and_percentiles() {
        assert_eq!(bucket(4.99), "under_5s");
        assert_eq!(bucket(20.0), "5_to_20s");
        assert_eq!(bucket(60.0), "20_to_60s");
        assert_eq!(bucket(60.1), "over_60s");
        assert_eq!(percentile(&[3., 1., 2.], 0.5), 2.);
        assert_eq!(percentile(&[3., 1., 2.], 0.95), 3.);
    }
}
