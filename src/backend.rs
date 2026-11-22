use crate::config::Config;
use crate::process::{CHILD_GRACE, ChildGroup, nonblocking, run_command_until, wait_for_io};
use crate::segments::split_at_pauses;
use crate::setup::MODEL_FILES;
use crate::vocabulary::hotwords;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
const SANDBOX_ARGS: &[&str] = &[
    "--unshare-net",
    "--unshare-pid",
    "--dev-bind",
    "/",
    "/",
    "--proc",
    "/proc",
    "--",
];

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
    let mut audio = hound::WavReader::open(path).context("Could not read PCM WAV")?;
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

    pub fn ensure(&self) -> Result<()> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let mut worker = self.worker.lock().unwrap();
        ensure!(!self.cancelled(epoch), "Speech backend startup cancelled");
        if let Some(active) = worker.as_mut()
            && active.epoch == epoch
            && !active.group.exited()?
        {
            return Ok(());
        }
        *worker = None;
        *worker = Some(self.start(epoch)?);
        Ok(())
    }

    fn start(&self, epoch: u64) -> Result<Worker> {
        let config = &self.config;
        let model = Path::new(&config.model);
        ensure!(
            Path::new(&config.server).is_file()
                && MODEL_FILES.iter().all(|name| model.join(name).is_file()),
            "Parakeet is not installed. Open Speech model and download the model."
        );
        ensure!(
            matches!(config.language.as_str(), "en" | "auto"),
            "Parakeet Unified supports English only. Choose English in settings."
        );

        let cancelled = || self.cancelled(epoch);
        let sandbox = run_command_until(
            Command::new("bwrap").args(SANDBOX_ARGS).arg("true"),
            None,
            Duration::from_secs(5),
            true,
            cancelled,
        )
        .is_ok_and(|output| output.status.success());
        ensure!(!cancelled(), "Speech backend startup cancelled");
        let mut command = if sandbox {
            let mut command = Command::new("bwrap");
            command.args(SANDBOX_ARGS).arg(&config.server);
            command
        } else {
            Command::new(&config.server)
        };
        command.arg(model).arg(config.threads.to_string());
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
        let epoch = self.epoch.load(Ordering::SeqCst);
        ensure!(
            pcm.len().is_multiple_of(2) && pcm.len() <= PCM_LIMIT,
            "Invalid PCM16 audio length"
        );
        if !pcm.iter().any(|byte| *byte != 0) {
            return Ok(String::new());
        }
        let cancelled = || self.cancelled(epoch);
        let mut worker = self.worker.lock().unwrap();
        ensure!(!cancelled(), "Speech inference cancelled");
        let active = worker
            .as_mut()
            .filter(|active| active.epoch == epoch)
            .context("Speech backend is not running")?;
        let result = (|| {
            let mut transcript = String::new();
            for segment in split_at_pauses(pcm) {
                if !segment.iter().any(|byte| *byte != 0) {
                    continue;
                }
                let deadline = Instant::now() + DECODE_TIMEOUT;
                let header = ((segment.len() / 2) as u32).to_le_bytes();
                active.write(&header, deadline, &cancelled)?;
                active.write(segment, deadline, &cancelled)?;
                let response = active.response(deadline, &cancelled)?;
                let text = response
                    .get("text")
                    .and_then(Value::as_str)
                    .context("Speech backend returned no transcript")?;
                for word in text
                    .split(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
                    .filter(|word| !word.is_empty())
                {
                    if !transcript.is_empty() {
                        transcript.push(' ');
                    }
                    transcript.push_str(word);
                }
            }
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
