#[path = "evaluate/metrics.rs"]
mod metrics;

use anyhow::{Context, Result, bail, ensure};
use hype_flow::{
    backend::{Engine, RATE, Transcription, read_wav},
    cleanup,
    config::{Config, runtime_dir},
    vocabulary,
};
use metrics::{Keywords, Score, bucket, formatted, percentile, whisper, words};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};
use tempfile::Builder;

const USAGE: &str =
    "Usage: hype-flow-evaluate <manifest> [--config <path>] [--normalizer strict|whisper|formatted]
    [--max-wer <fraction>] [--boosting on|off] [--boost-score <score>] [--repeats <N>]
    [--set key=value] [--summary <path>]

Evaluate local mono 16 kHz PCM16 WAV recordings using the offline dictation engine.
JSONL manifest: {\"audio\": \"relative.wav\", \"text\": \"reference\", \"tags\": [\"quiet\"]}.
All three scores are reported. --normalizer selects the --max-wer gate (default strict).
Default config is Config::default(), never live preferences. --config pins TOML settings.
--set overrides a TOML value, e.g. --set threads=2 or --set noise_suppression=true.
Latency excludes loading, audio reads and scoring; minimum of N decode+cleanup repeats.
Reported real_time_factor follows the plan: audio seconds / processing seconds (higher is faster).";

struct Options {
    manifest: PathBuf,
    config: Option<PathBuf>,
    summary: Option<PathBuf>,
    max_wer: Option<f64>,
    boosting: Option<bool>,
    boost_score: Option<f64>,
    repeats: usize,
    normalizer: String,
    overrides: Vec<String>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            manifest: PathBuf::new(),
            config: None,
            summary: None,
            max_wer: None,
            boosting: None,
            boost_score: None,
            repeats: 1,
            normalizer: "strict".into(),
            overrides: Vec::new(),
        }
    }
}
#[derive(Deserialize)]
struct Recording {
    audio: String,
    text: String,
    #[serde(default)]
    tags: Vec<String>,
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
            let (flag, inline) = text.split_once('=').unwrap_or((text, ""));
            let value = if text.contains('=') {
                inline.to_owned()
            } else {
                arguments
                    .next()
                    .with_context(|| format!("{flag} requires a value"))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{flag} requires UTF-8 text"))?
            };
            match flag {
                "--config" => options.config = Some(value.into()),
                "--summary" => options.summary = Some(value.into()),
                "--normalizer" => {
                    ensure!(
                        matches!(value.as_str(), "strict" | "whisper" | "formatted"),
                        "Unknown normalizer"
                    );
                    options.normalizer = value;
                }
                "--repeats" => {
                    options.repeats = value.parse().context("Invalid --repeats")?;
                    ensure!(options.repeats > 0, "--repeats must be positive");
                }
                "--set" => options.overrides.push(value),
                "--boosting" => {
                    options.boosting = Some(match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => bail!("--boosting expects on or off"),
                    })
                }
                "--max-wer" => {
                    let number: f64 = value.parse().context("Invalid --max-wer")?;
                    ensure!(
                        number.is_finite() && number >= 0.0,
                        "--max-wer must be finite and non-negative"
                    );
                    options.max_wer = Some(number);
                }
                "--boost-score" => {
                    options.boost_score = Some(value.parse().context("Invalid --boost-score")?)
                }
                _ => bail!("Unknown option {flag}"),
            }
        } else {
            ensure!(
                options.manifest.as_os_str().is_empty(),
                "Expected one manifest"
            );
            options.manifest = argument.into();
        }
    }
    ensure!(!options.manifest.as_os_str().is_empty(), "Missing manifest");
    Ok(Some(options))
}

fn configuration(options: &Options) -> Result<Config> {
    let mut table = toml::Table::new();
    if let Some(path) = &options.config {
        for (key, value) in toml::from_str::<toml::Table>(&fs::read_to_string(path)?)? {
            table.insert(key, value);
        }
    }
    for value in &options.overrides {
        let supplied: toml::Table =
            toml::from_str(value).context("--set expects a TOML assignment")?;
        ensure!(supplied.len() == 1, "--set expects one assignment");
        for (key, value) in supplied {
            table.insert(key, value);
        }
    }
    let mut values = Config::from_table(table)?;
    if let Some(boosting) = options.boosting {
        values.vocabulary_boosting = boosting;
    }
    if let Some(score) = options.boost_score {
        values.boost_score = score;
    }
    values.validate()?;
    for path in [&mut values.model, &mut values.server] {
        if let Some(tail) = path.strip_prefix("~/") {
            *path = env::home_dir()
                .context("Cannot expand home directory")?
                .join(tail)
                .to_string_lossy()
                .into_owned();
        }
    }
    Ok(values)
}

fn fingerprint(path: &Path) -> Result<serde_json::Value> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Fingerprint {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(json!({"sha256":format!("{:x}",hash.finalize()),"bytes":file.metadata()?.len()}))
}

fn provenance(values: &Config) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut result = BTreeMap::new();
    result.insert("worker".into(), fingerprint(Path::new(&values.server))?);
    let libraries = Path::new(&values.server)
        .parent()
        .context("Worker path has no parent")?
        .join("lib");
    for name in ["libsherpa-onnx-c-api.so", "libonnxruntime.so"] {
        let path = libraries.join(name);
        if path.exists() {
            result.insert(name.into(), fingerprint(&path)?);
        }
    }
    if let Some(path) = values.provider.strip_prefix("cpu:") {
        result.insert("session_config".into(), fingerprint(Path::new(path))?);
    }

    for name in hype_flow::setup::model_files(Path::new(&values.model)) {
        result.insert(
            (*name).to_owned(),
            fingerprint(&Path::new(&values.model).join(name))?,
        );
    }
    if values.vocabulary_boosting {
        result.insert(
            "bpe.vocab".into(),
            fingerprint(&Path::new(&values.model).join("bpe.vocab"))?,
        );
    }
    let root = Path::new(&values.model)
        .parent()
        .context("Model directory has no parent")?;
    for (enabled, name) in [
        (values.cut_silence, "silero_vad.onnx"),
        (values.noise_suppression, "gtcrn_simple.onnx"),
    ] {
        if enabled {
            result.insert(name.into(), fingerprint(&root.join(name))?);
        }
    }
    Ok(result)
}

fn dataset_macro_scores(
    by_tag: &BTreeMap<String, BTreeMap<&str, Score>>,
) -> BTreeMap<&'static str, f64> {
    let datasets = [
        "ami",
        "earnings22",
        "gigaspeech",
        "librispeech_clean",
        "librispeech_other",
        "spgispeech",
        "voxpopuli",
    ];
    let mut means = BTreeMap::new();
    for mode in ["strict", "whisper", "formatted"] {
        let rates = datasets
            .iter()
            .filter_map(|name| {
                by_tag
                    .get(*name)?
                    .get(mode)
                    .map(|score| score.word_error_rate)
            })
            .collect::<Vec<_>>();
        if !rates.is_empty() {
            means.insert(mode, rates.iter().sum::<f64>() / rates.len() as f64);
        }
    }
    means
}

fn clean_transcription(transcript: &Transcription, values: &Config) -> Result<String> {
    if values.hallucination_filtering && transcript.anomaly_score >= values.anomaly_threshold {
        return Ok(String::new());
    }
    cleanup::process_with_confidence(&transcript.text, values, &transcript.words)
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
    let values = configuration(&options)?;
    let provenance = provenance(&values)?;
    let terms = vocabulary::terms(&values.prompt)?
        .iter()
        .map(|term| words(term))
        .collect::<Vec<_>>();
    let engine = Engine::new(values.clone(), directory.path().to_owned());
    let base = options.manifest.parent().unwrap_or_else(|| Path::new("."));
    let mut output = io::stdout().lock();
    let mut scores: BTreeMap<&str, Score> = BTreeMap::new();
    let mut raw_scores: BTreeMap<&str, Score> = BTreeMap::new();
    let mut by_tag: BTreeMap<String, BTreeMap<&str, Score>> = BTreeMap::new();
    let mut by_length: BTreeMap<&str, BTreeMap<&str, Score>> = BTreeMap::new();
    let mut keywords = Keywords::default();
    let mut times = Vec::new();
    let mut audio_seconds = 0.0;
    let mut confidence_words = 0;
    let mut low_confidence_words = 0;
    let mut anomalous_cases = 0;
    let mut unstable_cases = 0;
    let load_start = Instant::now();
    engine.ensure()?;
    let load_seconds = load_start.elapsed().as_secs_f64();
    for case in &cases {
        let pcm = read_wav(&base.join(&case.audio))?;
        let seconds = pcm.len() as f64 / (RATE * 2) as f64;
        let mut fastest = f64::INFINITY;
        let mut raw = String::new();
        let mut transcript = String::new();
        let mut stable = true;
        let mut detailed = Transcription::default();
        for repeat in 0..options.repeats {
            let start = Instant::now();
            let current_detailed = engine.transcribe_detailed(&pcm)?;
            let current = clean_transcription(&current_detailed, &values)?;
            let current_raw = &current_detailed.text;
            let elapsed = start.elapsed().as_secs_f64();
            if repeat > 0 {
                stable &= current == transcript && *current_raw == raw;
            }
            if elapsed < fastest {
                fastest = elapsed;
                transcript = current;
                raw = current_raw.clone();
                detailed = current_detailed;
            }
        }
        unstable_cases += usize::from(!stable);
        let low_words = detailed
            .words
            .iter()
            .filter(|word| word.min_log_probability < values.confidence_threshold)
            .count();
        confidence_words += detailed.words.len();
        low_confidence_words += low_words;
        anomalous_cases += usize::from(detailed.anomaly_score >= values.anomaly_threshold);
        let normalized = whisper(&[&case.text, &raw, &transcript])?;
        let modes = [
            ("strict", words(&case.text), words(&raw), words(&transcript)),
            (
                "whisper",
                normalized[0].clone(),
                normalized[1].clone(),
                normalized[2].clone(),
            ),
            (
                "formatted",
                formatted(&case.text),
                formatted(&raw),
                formatted(&transcript),
            ),
        ];
        let mut case_scores = BTreeMap::new();
        let mut case_raw_scores = BTreeMap::new();
        for (mode, reference, original, cleaned) in modes {
            scores.entry(mode).or_default().add(&reference, &cleaned);
            raw_scores
                .entry(mode)
                .or_default()
                .add(&reference, &original);
            by_length
                .entry(bucket(seconds))
                .or_default()
                .entry(mode)
                .or_default()
                .add(&reference, &cleaned);
            let mut tags = case.tags.clone();
            tags.sort();
            tags.dedup();
            for tag in tags {
                by_tag
                    .entry(tag)
                    .or_default()
                    .entry(mode)
                    .or_default()
                    .add(&reference, &cleaned);
            }
            let mut score = Score::default();
            score.add(&reference, &cleaned);
            case_scores.insert(mode, score);
            let mut score = Score::default();
            score.add(&reference, &original);
            case_raw_scores.insert(mode, score);
        }
        keywords.add(&terms, &words(&case.text), &words(&transcript));
        times.push(fastest);
        audio_seconds += seconds;
        writeln!(
            output,
            "{}",
            json!({"type":"case", "audio":case.audio,"tags":case.tags,"text":transcript,"raw_text":raw,
            "errors":case_scores["strict"].errors,"raw_errors":case_raw_scores["strict"].errors,
            "words":case_scores["strict"].reference_words,"seconds":fastest,"audio_seconds":seconds,
            "real_time_factor":seconds/fastest,"repeat_transcripts_stable":stable,
            "confidence_words":detailed.words.len(),"low_confidence_words":low_words,"anomaly_score":detailed.anomaly_score,
            "scores":case_scores,"raw_scores":case_raw_scores,"peak_worker_rss_kib":engine.peak_worker_rss_kib()})
        )?;
        output.flush()?;
    }
    let peak_worker_rss_kib = engine.peak_worker_rss_kib();
    engine.stop();
    let seconds: f64 = times.iter().sum();
    let macro_dataset_word_error_rate = dataset_macro_scores(&by_tag);
    let summary = json!({"type":"summary","provenance":provenance,"manifest_sha256":format!("{:x}",Sha256::digest(contents.as_bytes())),"config":values,"normalizer":options.normalizer,"repeats":options.repeats,
        "cases":cases.len(),"confidence_words":confidence_words,"low_confidence_words":low_confidence_words,"anomalous_cases":anomalous_cases,"unstable_repeat_cases":unstable_cases,"word_error_rate":scores["strict"].word_error_rate,
        "raw_word_error_rate":raw_scores["strict"].word_error_rate,"errors":scores["strict"].errors,
        "reference_words":scores["strict"].reference_words,"scores":scores,"raw_scores":raw_scores,
        "keywords":keywords,"macro_dataset_word_error_rate":macro_dataset_word_error_rate,"by_tag":by_tag,"by_length":by_length,"load_seconds":load_seconds,
        "p50_seconds":percentile(&times,0.5),"p95_seconds":percentile(&times,0.95),
        "audio_seconds":audio_seconds,"processing_seconds":seconds,"real_time_factor":audio_seconds/seconds,
        "peak_worker_rss_kib":peak_worker_rss_kib});
    writeln!(output, "{summary}")?;
    if let Some(path) = &options.summary {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&summary)?),
        )?;
    }
    Ok(options
        .max_wer
        .is_some_and(|max| scores[options.normalizer.as_str()].word_error_rate > max))
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
    fn pinned_configuration_and_overrides_are_validated() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("eval.toml");
        fs::write(&path, "threads = 3\nprompt = 'Pinned vocabulary'\n").unwrap();
        let mut options = Options {
            config: Some(path.clone()),
            overrides: vec!["threads=2".into()],
            ..Options::default()
        };
        let config = configuration(&options).unwrap();
        assert_eq!(config.threads, 2);
        assert_eq!(config.prompt, "Pinned vocabulary");
        options.overrides = vec!["threads=0".into()];
        assert!(configuration(&options).is_err());
        options.overrides.clear();
        fs::write(&path, "unknown_setting = true\n").unwrap();
        assert!(configuration(&options).is_err());
    }

    #[test]
    fn dataset_macro_average_does_not_weight_larger_sets() {
        let mut clean = Score::default();
        clean.add(&words("a"), &words("b"));
        let mut ami = Score::default();
        ami.add(&vec!["a".into(); 99], &vec!["a".into(); 99]);
        let mut tags = BTreeMap::new();
        tags.insert(
            "librispeech_clean".into(),
            BTreeMap::from([("whisper", clean)]),
        );
        tags.insert("ami".into(), BTreeMap::from([("whisper", ami)]));
        assert_eq!(dataset_macro_scores(&tags)["whisper"], 0.5);
    }

    #[test]
    fn evaluation_applies_confidence_and_anomaly_controls_to_detailed_results() {
        let mut values = Config {
            prompt: "Sinead".into(),
            fuzzy_vocabulary: true,
            confidence_filtering: true,
            ..Config::default()
        };
        let mut transcript = Transcription {
            text: "Shunade".into(),
            words: vec![hype_flow::backend::WordConfidence {
                text: "Shunade".into(),
                min_log_probability: -0.1,
                ..Default::default()
            }],
            anomaly_score: 4.0,
            ..Default::default()
        };
        assert_eq!(
            clean_transcription(&transcript, &values).unwrap(),
            "Shunade"
        );
        transcript.words[0].min_log_probability = -2.0;
        assert_eq!(clean_transcription(&transcript, &values).unwrap(), "Sinead");
        values.hallucination_filtering = true;
        assert_eq!(clean_transcription(&transcript, &values).unwrap(), "");
        assert_eq!(transcript.text, "Shunade");
    }

    #[test]
    fn documented_evaluation_configuration_loads() {
        let options = Options {
            config: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/eval.toml")),
            ..Options::default()
        };
        let config = configuration(&options).unwrap();
        assert!(!config.fuzzy_vocabulary);
        assert!(!config.vocabulary_boosting);
        assert_eq!(config.threads, 4);
    }
}
