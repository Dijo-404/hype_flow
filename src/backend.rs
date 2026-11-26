use crate::config::Config;
use crate::process::{CHILD_GRACE, ChildGroup, nonblocking, run_command_until, wait_for_io};
use crate::segments::{ctc_preroll, split_at_pauses_with_limit};
use crate::setup::{model_files, variant_for_path};
use crate::vocabulary::hotwords;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

pub const RATE: usize = 16000;
pub const MAX_SECONDS: usize = 300;
pub(crate) const PCM_LIMIT: usize = RATE * MAX_SECONDS * 2;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
const DECODE_TIMEOUT: Duration = Duration::from_secs(600);
static SANDBOX_WORKS: AtomicBool = AtomicBool::new(false);

fn bubblewrap(reads: &[&Path]) -> Command {
    let mut command = Command::new("bwrap");
    command.args([
        "--unshare-net",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]);
    // Path sockets such as the session bus still connect across a network namespace.
    let shared = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir());
    if let Some(shared) = &shared {
        command.arg("--tmpfs").arg(shared);
    }
    // Later mounts win: re-expose what the recognizer reads, never a hidden root itself.
    for path in reads {
        if !Path::new("/tmp").starts_with(path)
            && shared
                .as_ref()
                .is_none_or(|shared| !shared.starts_with(path))
        {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    command.arg("--");
    command
}

pub fn wav_bytes(pcm: &[u8]) -> Vec<u8> {
    let size = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&[16, 0, 0, 0, 1, 0, 1, 0]);
    wav.extend_from_slice(&(RATE as u32).to_le_bytes());
    wav.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&[2, 0, 16, 0]);
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

pub fn read_wav(path: &Path) -> Result<Vec<u8>> {
    pcm_from_wav(File::open(path).context("Could not read PCM WAV")?)
}

pub(crate) fn pcm_from_wav(reader: impl Read) -> Result<Vec<u8>> {
    let mut audio =
        hound::WavReader::new(io::BufReader::new(reader)).context("Could not read PCM WAV")?;
    let spec = audio.spec();
    ensure!(
        spec.channels == 1
            && spec.bits_per_sample == 16
            && spec.sample_rate as usize == RATE
            && spec.sample_format == hound::SampleFormat::Int,
        "Expected mono, 16 kHz, signed 16-bit PCM WAV. Convert with ffmpeg first."
    );
    let samples = audio.duration() as usize;
    ensure!(
        samples <= RATE * MAX_SECONDS,
        "Audio exceeds {MAX_SECONDS} seconds"
    );
    let mut pcm = Vec::with_capacity(samples * 2);
    for sample in audio.samples::<i16>() {
        pcm.extend_from_slice(&sample?.to_le_bytes());
    }
    Ok(pcm)
}

/// A recognized word with conservative minimum token confidence.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WordConfidence {
    pub text: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub min_log_probability: f64,
    pub duration_is_reliable: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Transcription {
    pub text: String,
    pub audio_seconds: f64,
    pub words: Vec<WordConfidence>,
    pub anomaly_score: f64,
}

impl Transcription {
    pub fn append(&mut self, mut other: Self, offset_seconds: f64) {
        if !self.text.is_empty() && !other.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(&other.text);
        self.audio_seconds = self.audio_seconds.max(offset_seconds + other.audio_seconds);
        for word in &mut other.words {
            word.start_seconds += offset_seconds;
            word.end_seconds += offset_seconds;
        }
        self.words.extend(other.words);
        self.anomaly_score = anomaly_score(&self.words);
    }
}

fn anomaly_score(words: &[WordConfidence]) -> f64 {
    // faster-whisper's word probability/duration anomaly heuristic; opt-in only.
    words
        .iter()
        .take(8)
        .map(|word| {
            let duration = (word.end_seconds - word.start_seconds).max(0.0);
            f64::from(word.min_log_probability.exp() < 0.15)
                + if word.duration_is_reliable && duration < 0.133 {
                    (0.133 - duration) * 15.0
                } else {
                    0.0
                }
                + if word.duration_is_reliable && duration > 2.0 {
                    duration - 2.0
                } else {
                    0.0
                }
        })
        .sum()
}

fn transcription(response: &Value, audio_seconds: f64) -> Result<Transcription> {
    let text = response
        .get("text")
        .and_then(Value::as_str)
        .context("Speech backend returned no transcript")?
        .split(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut result = Transcription {
        text,
        audio_seconds,
        ..Default::default()
    };
    if let Some(chunks) = response.get("chunks").and_then(Value::as_array) {
        for chunk in chunks {
            let offset = chunk
                .get("audio_offset_seconds")
                .and_then(Value::as_f64)
                .filter(|offset| offset.is_finite() && *offset >= 0.0)
                .unwrap_or(0.0);
            let duration = chunk
                .get("audio_duration_seconds")
                .and_then(Value::as_f64)
                .filter(|duration| {
                    duration.is_finite()
                        && *duration >= 0.0
                        && offset + duration <= audio_seconds + 0.1
                });
            // Older workers omit per-piece duration; never invent a confidence range.
            let Some(duration) = duration else {
                continue;
            };
            let decoded = transcription(&chunk["result"], duration)?;
            result
                .words
                .extend(decoded.words.into_iter().map(|mut word| {
                    word.start_seconds += offset;
                    word.end_seconds += offset;
                    word
                }));
        }
    } else if let (Some(tokens), Some(times), Some(probabilities)) = (
        response.get("tokens").and_then(Value::as_array),
        response.get("timestamps").and_then(Value::as_array),
        response.get("ys_log_probs").and_then(Value::as_array),
    ) && tokens.len() == times.len()
        && tokens.len() == probabilities.len()
    {
        let mut previous_time = 0.0;
        let mut valid = true;
        for index in 0..tokens.len() {
            let Some(token) = tokens[index].as_str() else {
                valid = false;
                break;
            };
            let Some(time) = times[index]
                .as_f64()
                .filter(|t| t.is_finite() && *t >= previous_time && *t <= audio_seconds + 0.1)
            else {
                valid = false;
                break;
            };
            let Some(log_probability) = probabilities[index]
                .as_f64()
                .filter(|p| p.is_finite() && *p <= 0.0)
            else {
                valid = false;
                break;
            };
            previous_time = time;
            let native_duration = response
                .get("durations")
                .and_then(Value::as_array)
                .and_then(|durations| durations.get(index))
                .and_then(Value::as_f64)
                .filter(|duration| {
                    duration.is_finite()
                        && *duration >= 0.0
                        && time + duration <= audio_seconds + 0.1
                });
            let duration_is_reliable = native_duration.is_some()
                && response
                    .get("durations_reliable")
                    .and_then(Value::as_array)
                    .is_none_or(|flags| flags.get(index).and_then(Value::as_bool) == Some(true));
            let end = native_duration
                .map(|duration| time + duration)
                .unwrap_or_else(|| {
                    times
                        .get(index + 1)
                        .and_then(Value::as_f64)
                        .unwrap_or(audio_seconds)
                })
                .max(time)
                .min(audio_seconds.max(time));
            let begins_word = token.starts_with('▁') || token.starts_with(char::is_whitespace);
            let token = token.trim_start_matches('▁').trim();
            if token.is_empty() {
                continue;
            }
            if begins_word || result.words.is_empty() {
                result.words.push(WordConfidence {
                    text: token.to_owned(),
                    start_seconds: time,
                    end_seconds: end,
                    min_log_probability: log_probability,
                    duration_is_reliable,
                });
            } else if let Some(word) = result.words.last_mut() {
                word.text.push_str(token);
                word.end_seconds = end;
                word.min_log_probability = word.min_log_probability.min(log_probability);
                word.duration_is_reliable &= duration_is_reliable;
            }
        }
        if !valid {
            result.words.clear();
        }
    }
    result.anomaly_score = anomaly_score(&result.words);
    Ok(result)
}

struct Worker {
    group: ChildGroup,
    stdin: ChildStdin,
    stdout: ChildStdout,
    _hotwords: Option<NamedTempFile>,
    epoch: u64,
}

impl Worker {
    fn response(&mut self, deadline: Instant, cancelled: &impl Fn() -> bool) -> Result<Value> {
        let mut header = [0; 4];
        self.read(&mut header, deadline, cancelled)?;
        let size = u32::from_le_bytes(header) as usize;
        ensure!(
            (1..=RESPONSE_LIMIT).contains(&size),
            "Invalid response size from speech backend"
        );
        let mut body = vec![0; size];
        self.read(&mut body, deadline, cancelled)?;
        let response: Value =
            serde_json::from_slice(&body).context("Invalid speech backend response")?;
        ensure!(response.is_object(), "Invalid speech backend response");
        Ok(response)
    }

    fn read(
        &mut self,
        mut output: &mut [u8],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<()> {
        while !output.is_empty() {
            wait_for_io(self.stdout.as_raw_fd(), libc::POLLIN, deadline, cancelled)?;
            match self.stdout.read(output) {
                Ok(0) => bail!("Speech backend exited"),
                Ok(length) => output = &mut output[length..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn write(
        &mut self,
        mut input: &[u8],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<()> {
        while !input.is_empty() {
            wait_for_io(self.stdin.as_raw_fd(), libc::POLLOUT, deadline, cancelled)?;
            match self.stdin.write(input) {
                Ok(0) => bail!("Speech backend closed its input"),
                Ok(length) => input = &input[length..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("Speech backend closed its input"),
            }
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.group.terminate(CHILD_GRACE);
    }
}

pub struct Engine {
    config: Config,
    runtime: PathBuf,
    worker: Mutex<Option<Worker>>,
    // Even epochs allow I/O. Incrementing twice across stop/prepare keeps old I/O cancelled.
    epoch: AtomicU64,
}

impl Engine {
    pub fn new(config: Config, runtime: PathBuf) -> Self {
        Self {
            config,
            runtime,
            worker: Mutex::new(None),
            epoch: AtomicU64::new(0),
        }
    }

    pub(crate) fn normalizes_ctc_preroll(&self) -> bool {
        self.config.cut_silence && variant_for_path(Path::new(&self.config.model)) == "parakeet-ctc"
    }

    pub fn prepare(&self) {
        let epoch = self.epoch.load(Ordering::SeqCst);
        if epoch & 1 != 0 {
            let _ = self.epoch.compare_exchange(
                epoch,
                epoch.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    fn cancelled(&self, epoch: u64) -> bool {
        epoch & 1 != 0 || self.epoch.load(Ordering::SeqCst) != epoch
    }

    pub(crate) fn recording_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn ensure(&self) -> Result<()> {
        self.ensure_scoped(self.recording_epoch(), &|| false)
    }

    pub(crate) fn ensure_scoped(
        &self,
        epoch: u64,
        external_cancelled: &impl Fn() -> bool,
    ) -> Result<()> {
        let mut worker = self.worker.lock().unwrap();
        ensure!(
            !self.cancelled(epoch) && !external_cancelled(),
            "Speech backend startup cancelled"
        );
        if let Some(active) = worker.as_mut()
            && active.epoch == epoch
            && !active.group.exited()?
        {
            return Ok(());
        }
        *worker = None;
        *worker = Some(self.start(epoch, external_cancelled)?);
        Ok(())
    }

    fn start(&self, epoch: u64, external_cancelled: &impl Fn() -> bool) -> Result<Worker> {
        let config = &self.config;
        let model = Path::new(&config.model);
        ensure!(
            Path::new(&config.server).is_file()
                && model_files(model)
                    .iter()
                    .all(|name| model.join(name).is_file()),
            "Parakeet is not installed. Open Speech model and download the model."
        );
        ensure!(
            matches!(config.language.as_str(), "en" | "auto"),
            "Choose English in settings to use this speech model."
        );

        let cancelled = || self.cancelled(epoch) || external_cancelled();
        let server = Path::new(&config.server);
        // Parents also cover the VAD and denoiser beside the model and the worker's lib/.
        let reads = [
            self.runtime.as_path(),
            model.parent().unwrap_or(model),
            server.parent().unwrap_or(server),
        ];
        let sandbox = SANDBOX_WORKS.load(Ordering::Relaxed)
            || run_command_until(
                bubblewrap(&reads).arg("true"),
                None,
                Duration::from_secs(5),
                true,
                cancelled,
            )
            .is_ok_and(|output| output.status.success());
        // Remember only success, so a transient failure is probed again next start.
        SANDBOX_WORKS.fetch_or(sandbox, Ordering::Relaxed);
        ensure!(!cancelled(), "Speech backend startup cancelled");
        let mut command = if sandbox {
            let mut command = bubblewrap(&reads);
            command.arg(server);
            command
        } else {
            Command::new(&config.server)
        };
        command.arg(model).arg(config.threads.to_string());
        if variant_for_path(model) == "parakeet-ctc" {
            command.arg("--ctc");
        }
        command.arg("--warmup");
        if config.normalize_gain {
            command.arg("--normalize-gain");
        }
        if config.soft_speech_retry {
            command.arg("--soft-speech-retry");
        }
        if config.feature_dim != 128 {
            command
                .arg("--feature-dim")
                .arg(config.feature_dim.to_string());
        }
        if config.provider != "cpu" {
            command.arg("--provider").arg(&config.provider);
        }
        for (flag, value) in [
            ("--blank-penalty", config.blank_penalty.to_string()),
            ("--max-active-paths", config.max_active_paths.to_string()),
            ("--vad-threshold", config.vad_threshold.to_string()),
            ("--vad-min-silence", config.vad_min_silence.to_string()),
            ("--max-piece-seconds", config.max_piece_seconds.to_string()),
        ] {
            command.arg(flag).arg(value);
        }
        let mut context_file = None;
        if config.vocabulary_boosting {
            let vocabulary = hotwords(&config.prompt)?;
            if !vocabulary.is_empty() {
                ensure!(
                    model.join("bpe.vocab").is_file(),
                    "Vocabulary support needs an update. Open Speech model and update speech support."
                );
                let mut file = tempfile::Builder::new()
                    .prefix("context-vocabulary-")
                    .tempfile_in(&self.runtime)?;
                file.write_all(vocabulary.as_bytes())?;
                command
                    .arg("--hotwords")
                    .arg(file.path())
                    .arg(config.boost_score.to_string());
                context_file = Some(file);
            }
        }
        for (enabled, flag, name) in [
            (config.cut_silence, "--vad", "silero_vad.onnx"),
            (config.noise_suppression, "--denoise", "gtcrn_simple.onnx"),
        ] {
            if enabled {
                let path = model.parent().unwrap_or_else(|| Path::new("")).join(name);
                ensure!(
                    path.is_file(),
                    "Silence cutting and noise suppression need an update. Open Speech model and update speech support."
                );
                command.arg(flag).arg(path);
            }
        }

        let log_path = self.runtime.join("backend.log");
        let mut log = NamedTempFile::new_in(&self.runtime)?;
        writeln!(
            log,
            "{}",
            if sandbox {
                "Network sandbox: bubblewrap; the recognizer has no network access"
            } else {
                "Network sandbox: unavailable (install bubblewrap with user namespaces enabled)"
            }
        )?;
        let log = log
            .persist(&log_path)
            .context("Could not create private backend log")?;
        ensure!(!cancelled(), "Speech backend startup cancelled");
        let mut group = ChildGroup::spawn(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(log),
        )?;
        let stdin = group.child.stdin.take().expect("stdin is piped");
        let stdout = group.child.stdout.take().expect("stdout is piped");
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        let mut worker = Worker {
            group,
            stdin,
            stdout,
            _hotwords: context_file,
            epoch,
        };
        let result = worker
            .response(Instant::now() + LOAD_TIMEOUT, &cancelled)
            .with_context(|| {
                format!("Speech backend startup failed. See {}", log_path.display())
            })?;
        ensure!(
            result == serde_json::json!({"ready": true}),
            "Speech backend did not become ready"
        );
        ensure!(!cancelled(), "Speech backend startup cancelled");
        Ok(worker)
    }

    pub fn transcribe(&self, pcm: &[u8]) -> Result<String> {
        self.transcribe_detailed(pcm).map(|result| result.text)
    }

    pub fn transcribe_detailed(&self, pcm: &[u8]) -> Result<Transcription> {
        self.transcribe_detailed_scoped(pcm, self.recording_epoch(), &|| false)
    }

    pub(crate) fn transcribe_detailed_scoped(
        &self,
        pcm: &[u8],
        epoch: u64,
        external_cancelled: &impl Fn() -> bool,
    ) -> Result<Transcription> {
        ensure!(
            pcm.len().is_multiple_of(2) && pcm.len() <= PCM_LIMIT,
            "Invalid PCM16 audio length"
        );
        if !pcm.iter().any(|byte| *byte != 0) {
            return Ok(Transcription {
                audio_seconds: pcm.len() as f64 / (RATE * 2) as f64,
                ..Default::default()
            });
        }
        let audio_seconds = pcm.len() as f64 / (RATE * 2) as f64;
        let (pcm, source_offset) = if self.normalizes_ctc_preroll() {
            ctc_preroll(pcm)
        } else {
            (pcm.into(), 0)
        };
        let cancelled = || self.cancelled(epoch) || external_cancelled();
        let mut worker = self.worker.lock().unwrap();
        ensure!(!cancelled(), "Speech inference cancelled");
        let active = worker
            .as_mut()
            .filter(|active| active.epoch == epoch)
            .context("Speech backend is not running")?;
        let result = (|| {
            let mut transcript = Transcription::default();
            let mut audio_offset = source_offset as f64 / (RATE * 2) as f64;
            for segment in split_at_pauses_with_limit(&pcm, self.config.max_piece_seconds as usize)
            {
                let segment_offset = audio_offset;
                audio_offset += segment.len() as f64 / (RATE * 2) as f64;
                if !segment.iter().any(|byte| *byte != 0) {
                    continue;
                }
                let deadline = Instant::now() + DECODE_TIMEOUT;
                let header = ((segment.len() / 2) as u32).to_le_bytes();
                active.write(&header, deadline, &cancelled)?;
                active.write(segment, deadline, &cancelled)?;
                let response = active.response(deadline, &cancelled)?;
                let mut decoded =
                    transcription(&response, segment.len() as f64 / (RATE * 2) as f64)?;
                if segment_offset < 0.0 {
                    for word in &mut decoded.words {
                        word.start_seconds = word.start_seconds.max(-segment_offset);
                        word.end_seconds = word.end_seconds.max(-segment_offset);
                    }
                }
                transcript.append(decoded, segment_offset);
            }
            transcript.audio_seconds = audio_seconds;
            ensure!(!cancelled(), "Speech inference cancelled");
            Ok(transcript)
        })();
        if result.is_err() {
            *worker = None;
        }
        if cancelled() {
            bail!("Speech inference cancelled");
        }
        result.with_context(|| {
            format!(
                "Speech inference failed. See {}",
                self.runtime.join("backend.log").display()
            )
        })
    }

    pub fn peak_worker_rss_kib(&self) -> Option<u64> {
        let worker = self.worker.lock().ok()?;
        let root = worker.as_ref()?.group.child.id();
        // bwrap owns the direct child PID; inspect descendants for the recognizer.
        fn peak(pid: u32) -> Option<u64> {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let own = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmHWM:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0);
            let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap_or_default();
            Some(
                children
                    .split_whitespace()
                    .filter_map(|child| child.parse().ok())
                    .filter_map(peak)
                    .fold(own, u64::max),
            )
        }
        peak(root).filter(|value| *value > 0)
    }

    pub fn stop(&self) {
        self.reap(self.cancel());
    }

    pub fn stop_in_background(self: &Arc<Self>) -> thread::JoinHandle<()> {
        let cancelled_epoch = self.cancel();
        let engine = Arc::clone(self);
        thread::spawn(move || engine.reap(cancelled_epoch))
    }

    fn cancel(&self) -> u64 {
        self.epoch.fetch_or(1, Ordering::SeqCst) & !1
    }

    fn reap(&self, cancelled_epoch: u64) {
        let mut worker = self.worker.lock().unwrap();
        if worker
            .as_ref()
            .is_some_and(|active| active.epoch <= cancelled_epoch)
        {
            *worker = None;
        }
    }
}

#[cfg(test)]
mod confidence_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subword_confidence_uses_minimum_and_preserves_original_time() {
        let decoded = transcription(
            &json!({"text":"OpenAI works.",
            "tokens":["▁Open","AI","▁works","."],
            "timestamps":[0.2,0.3,0.6,0.9],"durations":[0.1,0.1,0.2,0.02],
            "ys_log_probs":[-0.1,-1.8,-0.2,-0.3]}),
            1.0,
        )
        .unwrap();
        assert_eq!(decoded.words.len(), 2);
        assert_eq!(decoded.words[0].text, "OpenAI");
        assert_eq!(decoded.words[0].min_log_probability, -1.8);
        assert_eq!(decoded.words[1].text, "works.");
        assert_eq!(decoded.words[1].end_seconds, 0.92);
        let mut joined = Transcription::default();
        joined.append(decoded, 15.0);
        assert_eq!(joined.words[0].start_seconds, 15.2);
    }

    #[test]
    fn sherpa_space_prefixed_tokens_are_word_boundaries() {
        let decoded = transcription(
            &json!({"text":"And fellow Americans,",
            "tokens":[" And"," fe","llow"," A","mer","ic","ans",","],
            "timestamps":[0.0,0.2,0.2,0.5,0.5,0.6,0.7,0.8],
            "durations":[],"ys_log_probs":[-0.1,-0.1,-0.2,-0.1,-0.3,-0.1,-0.1,-0.4]}),
            1.0,
        )
        .unwrap();
        assert_eq!(
            decoded
                .words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["And", "fellow", "Americans,"]
        );
        assert_eq!(decoded.words[2].min_log_probability, -0.4);
        assert_eq!(decoded.audio_seconds, 1.0);
    }

    #[test]
    fn chunks_use_exact_duration_and_missing_confidence_stays_unknown() {
        let decoded = transcription(
            &json!({"text":"one two", "chunks":[
                {"audio_offset_seconds":2.0,"audio_duration_seconds":1.0,"result":{
                    "text":"one","tokens":["▁one"],"timestamps":[0.2],"ys_log_probs":[-0.1]}},
                {"audio_offset_seconds":6.0,"audio_duration_seconds":2.0,"result":{
                    "text":"two","tokens":["▁two"],"timestamps":[0.5],"ys_log_probs":[-0.2]}}
            ]}),
            10.0,
        )
        .unwrap();
        assert_eq!(decoded.words[0].start_seconds, 2.2);
        assert_eq!(decoded.words[0].end_seconds, 3.0);
        assert_eq!(decoded.words[1].start_seconds, 6.5);
        assert_eq!(decoded.words[1].end_seconds, 8.0);
        assert!(
            transcription(&json!({"text":"old worker"}), 1.0)
                .unwrap()
                .words
                .is_empty()
        );
        assert!(
            transcription(
                &json!({"text":"bad confidence","tokens":["▁bad"],
            "timestamps":[0.2],"ys_log_probs":[0.8]}),
                1.0
            )
            .unwrap()
            .words
            .is_empty()
        );
    }

    #[test]
    fn anomaly_requires_low_probability_or_implausible_duration() {
        let words = vec![
            WordConfidence {
                text: "noise".into(),
                start_seconds: 0.0,
                end_seconds: 0.02,
                min_log_probability: -3.0,
                duration_is_reliable: true,
            };
            3
        ];
        assert!(anomaly_score(&words) >= 3.0);
        let words = vec![
            WordConfidence {
                text: "speech".into(),
                start_seconds: 0.0,
                end_seconds: 0.3,
                min_log_probability: -0.1,
                duration_is_reliable: true,
            };
            3
        ];
        assert_eq!(anomaly_score(&words), 0.0);
    }

    #[test]
    fn silence_between_tokens_is_not_an_anomalous_word_duration() {
        let response = json!({"text":"hello world", "tokens":[" hello", " world"],
            "timestamps":[0.5, 6.0], "durations":[], "ys_log_probs":[-0.1, -0.1]});
        let decoded = transcription(&response, 6.5).unwrap();
        assert_eq!(decoded.words[0].end_seconds, 6.0);
        assert!(!decoded.words[0].duration_is_reliable);
        assert_eq!(decoded.anomaly_score, 0.0);

        let mut native = response.clone();
        native["durations"] = json!([5.5, 0.5]);
        assert!(transcription(&native, 6.5).unwrap().anomaly_score >= 3.0);
        native["durations_reliable"] = json!([false, true]);
        assert_eq!(transcription(&native, 6.5).unwrap().anomaly_score, 0.0);

        let mut low_probability = response;
        low_probability["ys_log_probs"] = json!([-3.0, -3.0]);
        assert_eq!(
            transcription(&low_probability, 6.5).unwrap().anomaly_score,
            2.0
        );
    }
}
