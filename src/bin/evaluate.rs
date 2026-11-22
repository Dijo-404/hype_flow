use anyhow::{Context, Result, bail, ensure};
use hype_flow::{
    backend::{Engine, read_wav},
    cleanup,
    config::{Config, runtime_dir},
    vocabulary::{casefold, whitespace},
};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::LazyLock,
    time::Instant,
};
use tempfile::Builder;
use unicode_normalization::UnicodeNormalization;

const USAGE: &str = "+Usage: hype-flow-evaluate <manifest> [--max-wer <fraction>] [--boosting <on|off>] [--boost-score <score>]

Evaluate local JSONL recordings using the same offline engine as dictation.
Each line contains an audio path relative to the manifest and its reference text.";
static PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{P}").unwrap());

#[derive(Default)]
struct Options {
    manifest: PathBuf,
    max_wer: Option<f64>,
    boosting: Option<bool>,
    boost_score: Option<f64>,
}

#[derive(Deserialize)]
struct Recording {
    audio: String,
    text: String,
}

fn options() -> Result<Option<Options>> {
    let mut options = Options::default();
    let mut arguments = env::args_os().skip(1);
    let mut positional = false;
    while let Some(argument) = arguments.next() {
        let text = argument.to_str().unwrap_or_default();
        if !positional && text == "--" {
            positional = true;
            continue;
        }
        if !positional && matches!(text, "-h" | "--help") {
            return Ok(None);
        }
        if !positional && text.len() > 1 && text.starts_with('-') {
            let (flag, value) = text.split_once('=').unwrap_or((text, ""));
            let value = if text.contains('=') {
                value.to_owned()
            } else {
                arguments
                    .next()
                    .with_context(|| format!("{flag} requires a value"))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{flag} requires UTF-8 text"))?
            };
            match flag {
                "--boosting" => {
                    options.boosting = Some(match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => bail!("--boosting expects on or off"),
                    });
                }
                "--max-wer" => options.max_wer = Some(value.parse().context("Invalid --max-wer")?),
                "--boost-score" => {
                    options.boost_score = Some(value.parse().context("Invalid --boost-score")?);
                }
                _ => bail!("Unknown option {flag}"),
            }
        } else {
            ensure!(
                options.manifest.as_os_str().is_empty(),
                "Expected one manifest"
            );
            options.manifest = PathBuf::from(argument);
        }
    }
    ensure!(!options.manifest.as_os_str().is_empty(), "Missing manifest");
    Ok(Some(options))
}

fn words(text: &str) -> Vec<String> {
    let normalized = casefold(&text.nfkc().collect::<String>());
    PUNCTUATION
        .replace_all(&normalized, " ")
        .split(whitespace)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn edits(reference: &[String], hypothesis: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    let mut current = vec![0; previous.len()];
    for (row, expected) in reference.iter().enumerate() {
        current[0] = row + 1;
        for (column, actual) in hypothesis.iter().enumerate() {
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + usize::from(expected != actual));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[hypothesis.len()]
}

fn evaluate(options: Options) -> Result<bool> {
    let contents = fs::read_to_string(&options.manifest)
        .with_context(|| format!("Could not read {}", options.manifest.display()))?;
    let cases = contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<Recording>(line)
                .with_context(|| format!("Invalid manifest line {}", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!cases.is_empty(), "Manifest contains no recordings");
    let directory = Builder::new()
        .prefix("evaluation-")
        .tempdir_in(runtime_dir()?)?;
    let mut values = Config::load()?;
    if let Some(boosting) = options.boosting {
        values.vocabulary_boosting = boosting;
    }
    if let Some(score) = options.boost_score {
        values.boost_score = score;
    }
    let base = options.manifest.parent().unwrap_or_else(|| Path::new("."));
    let engine = Engine::new(values.clone(), directory.path().to_owned());
    let mut output = io::stdout().lock();
    let mut errors = 0;
    let mut raw_errors = 0;
    let mut total_words = 0;
    engine.ensure()?;
    for case in cases {
        let pcm = read_wav(&base.join(&case.audio))?;
        let start = Instant::now();
        let raw = engine.transcribe(&pcm)?;
        let transcript = cleanup::process(&raw, &values)?;
        let reference = words(&case.text);
        let count = edits(&reference, &words(&transcript));
        let raw_count = edits(&reference, &words(&raw));
        errors += count;
        raw_errors += raw_count;
        total_words += reference.len();
        writeln!(
            output,
            "{}",
            json!({
                "audio": case.audio,
                "text": transcript,
                "raw_text": raw,
                "errors": count,
                "raw_errors": raw_count,
                "words": reference.len(),
                "seconds": (start.elapsed().as_secs_f64() * 100.0).round_ties_even() / 100.0,
            })
        )?;
        output.flush()?;
    }
    engine.stop();
    let denominator = total_words.max(1) as f64;
    let wer = errors as f64 / denominator;
    writeln!(
        output,
        "{}",
        json!({
            "word_error_rate": wer,
            "raw_word_error_rate": raw_errors as f64 / denominator,
            "errors": errors,
            "reference_words": total_words,
        })
    )?;
    Ok(options.max_wer.is_some_and(|maximum| wer > maximum))
}

fn main() -> ExitCode {
    match options() {
        Ok(Some(options)) => match evaluate(options) {
            Ok(exceeded) => ExitCode::from(u8::from(exceeded)),
            Err(error) => {
                eprintln!("hype-flow-evaluate: {error:#}");
                ExitCode::FAILURE
            }
        },
        Ok(None) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hype-flow-evaluate: {error:#}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_normalizes_unicode_and_keeps_indic_marks() {
        assert_eq!(words("Straße, ＡＢＣ! नमस्ते"), ["strasse", "abc", "नमस्ते"]);
        assert_eq!(edits(&words("a b c"), &words("a x c d")), 2);
        assert_eq!(edits(&words("a"), &words("")), 1);
        assert_eq!(edits(&words(""), &words("a")), 1);
    }
}
