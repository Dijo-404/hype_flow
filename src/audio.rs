use crate::backend::{Engine, PCM_LIMIT, RATE, Transcription};
use crate::process::{CHILD_GRACE, ChildGroup, POLL_INTERVAL, nonblocking, poll, run_command};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use unicode_casefold::UnicodeCaseFold;

const CHUNK: usize = 1600;
const PREROLL_BYTES: usize = RATE;
const HOLD_TIME: Duration = Duration::from_secs(30);
const FIRST_AUDIO_TIMEOUT: Duration = Duration::from_secs(5);
const DIAGNOSTIC_LIMIT: usize = 400;

type FrameCallback = Arc<dyn Fn(f64, f64) + Send + Sync>;
type EndCallback = Arc<dyn Fn() + Send + Sync>;
type FrameNotice = (FrameCallback, f64, f64);

pub fn sources() -> Vec<(String, String)> {
    let Ok(output) = run_command(&mut Command::new("pw-dump"), None, Duration::from_secs(3)) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(nodes) = serde_json::from_slice::<Vec<Value>>(&output.stdout) else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> = Vec::new();
    for node in nodes {
        let props = &node["info"]["props"];
        if node["type"] != "PipeWire:Interface:Node"
            || !matches!(
                props["media.class"].as_str(),
                Some("Audio/Source" | "Audio/Source/Virtual")
            )
        {
            continue;
        }
        if let Some(name) = props["node.name"].as_str().filter(|name| !name.is_empty()) {
            let description = props["node.description"]
                .as_str()
                .filter(|description| !description.is_empty())
                .unwrap_or(name)
                .to_owned();
            if let Some((_, previous)) = found.iter_mut().find(|(existing, _)| existing == name) {
                *previous = description;
            } else {
                found.push((name.to_owned(), description));
            }
        }
    }
    found.sort_by_cached_key(|(_, description)| description.case_fold().collect::<String>());
    found
}

struct Session {
    pcm: VecDeque<u8>,
    started: Instant,
    received: bool,
    ended: bool,
    recorded: usize,
    frame_bytes: usize,
    squares: u64,
    on_frame: FrameCallback,
    on_end: EndCallback,
    pieces: Option<(mpsc::SyncSender<Vec<u8>>, usize)>,
    spool: Option<File>,
    spool_failure: Option<String>,
}

impl Session {
    fn new(on_frame: FrameCallback, on_end: EndCallback) -> Self {
        Self {
            pcm: VecDeque::with_capacity(PCM_LIMIT),
            started: Instant::now(),
            received: false,
            ended: false,
            recorded: 0,
            frame_bytes: 0,
            squares: 0,
            on_frame,
            on_end,
            pieces: None,
            spool: None,
            spool_failure: None,
        }
    }

    fn end(&mut self) -> Option<EndCallback> {
        (!std::mem::replace(&mut self.ended, true)).then(|| Arc::clone(&self.on_end))
    }

    fn feed(&mut self, data: &[u8]) -> (Option<FrameNotice>, Option<EndCallback>) {
        if self.ended {
            return (None, None);
        }
        self.received = true;
        let accepted = data.len().min(PCM_LIMIT - self.recorded);
        let data = &data[..accepted];
        if let Some(spool) = &mut self.spool
            && let Err(error) = spool.write_all(data)
        {
            self.spool_failure = Some(format!("Could not save recovery audio: {error}"));
            return (None, self.end());
        }
        self.recorded += accepted;
        let overflow = (self.pcm.len() + accepted).saturating_sub(PCM_LIMIT);
        self.pcm.drain(..overflow);
        self.pcm.extend(data);
        if let Some((sender, maximum)) = &self.pieces
            && let Some(cut) = crate::segments::eager_cut(self.pcm.make_contiguous(), *maximum)
        {
            let piece = self.pcm.range(..cut).copied().collect();
            if sender.try_send(piece).is_ok() {
                self.pcm.drain(..cut);
            }
        }

        let (first, tail) = data.split_at((CHUNK - self.frame_bytes).min(accepted));
        self.squares += squares(first);
        self.frame_bytes += first.len();
        let frame = if self.frame_bytes == CHUNK {
            let rms = (self.squares as f64 / (CHUNK / 2) as f64).sqrt() / 32768.0;
            let duration = (self.recorded - tail.len()) as f64 / (RATE * 2) as f64;
            self.frame_bytes = tail.len();
            self.squares = squares(tail);
            Some((Arc::clone(&self.on_frame), (rms * 8.0).min(1.0), duration))
        } else {
            None
        };
        let end = if self.recorded == PCM_LIMIT {
            self.end()
        } else {
            None
        };
        (frame, end)
    }
}

fn squares(data: &[u8]) -> u64 {
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|sample| {
            let value = i64::from(i16::from_le_bytes(*sample));
            (value * value) as u64
        })
        .sum()
}

struct CaptureState {
    session: Option<Session>,
    ring: VecDeque<u8>,
    expires: Option<Instant>,
    closed: bool,
    failure: Option<String>,
}

struct Capture {
    target: String,
    state: Arc<Mutex<CaptureState>>,
    closing: Arc<AtomicBool>,
    input: Option<JoinHandle<()>>,
}

struct IdleCapture {
    capture: Option<Capture>,
    epoch: u64,
}

static IDLE: Mutex<IdleCapture> = Mutex::new(IdleCapture {
    capture: None,
    epoch: 0,
});

impl Capture {
    fn open(target: &str, session: Session) -> Result<Self> {
        let mut command = Command::new("pw-record");
        command.args([
            "--raw",
            "--format",
            "s16",
            "--rate",
            &RATE.to_string(),
            "--channels",
            "1",
            "--quality",
            "10",
            "--latency",
            "50ms",
            "--target",
            target,
            "--properties",
            "{ application.name = HypeFlow media.role = Communication }",
            "-",
        ]);
        let mut child = ChildGroup::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?;
        let stdout = child.child.stdout.take().expect("stdout is piped");
        let stderr = child.child.stderr.take().expect("stderr is piped");
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let state = Arc::new(Mutex::new(CaptureState {
            session: Some(session),
            ring: VecDeque::with_capacity(PREROLL_BYTES),
            expires: None,
            closed: false,
            failure: None,
        }));
        let closing = Arc::new(AtomicBool::new(false));
        let input = thread::Builder::new()
            .name("hype-flow-audio".into())
            .spawn({
                let state = Arc::clone(&state);
                let closing = Arc::clone(&closing);
                move || {
                    let error = read_capture(&mut child, stdout, stderr, &state, &closing).err();
                    let ended = {
                        let mut state = state.lock().unwrap();
                        state.closed = true;
                        state.ring.clear();
                        state.failure = error.map(|failure| format!("{failure:#}"));
                        if closing.load(Ordering::Acquire) {
                            None
                        } else {
                            state.session.as_mut().and_then(Session::end)
                        }
                    };
                    if let Some(ended) = ended {
                        ended();
                    }
                    child.terminate(CHILD_GRACE);
                }
            })?;
        Ok(Self {
            target: target.to_owned(),
            state,
            closing,
            input: Some(input),
        })
    }

    fn close(&mut self) -> Result<()> {
        self.closing.store(true, Ordering::Release);
        if let Some(input) = self.input.take()
            && input.thread().id() != thread::current().id()
            && input.join().is_err()
        {
            bail!("Microphone capture thread failed");
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn feed_capture(state: &Mutex<CaptureState>, data: &[u8]) {
    let (frame, ended) = {
        let mut state = state.lock().unwrap();
        if let Some(session) = &mut state.session {
            session.feed(data)
        } else {
            let overflow = (state.ring.len() + data.len()).saturating_sub(PREROLL_BYTES);
            state.ring.drain(..overflow);
            state.ring.extend(data);
            (None, None)
        }
    };
    if let Some((callback, volume, duration)) = frame {
        callback(volume, duration);
    }
    if let Some(callback) = ended {
        callback();
    }
}

fn read_available(pipe: &mut Option<impl Read>, buffer: &mut [u8]) -> io::Result<usize> {
    match pipe.as_mut().expect("polled pipe is open").read(buffer) {
        Ok(0) => {
            *pipe = None;
            Ok(0)
        }
        Ok(count) => Ok(count),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(0)
        }
        Err(error) => Err(error),
    }
}

fn read_capture(
    child: &mut ChildGroup,
    stdout: impl Read + AsRawFd,
    stderr: impl Read + AsRawFd,
    state: &Mutex<CaptureState>,
    closing: &AtomicBool,
) -> Result<()> {
    let mut stdout = Some(stdout);
    let mut stderr = Some(stderr);
    let mut diagnostics = VecDeque::new();
    let mut chunk = [0; CHUNK];
    let mut pending = 0;
    let mut eof_deadline = None;
    while !closing.load(Ordering::Acquire) {
        {
            let state = state.lock().unwrap();
            let now = Instant::now();
            if state.expires.is_some_and(|expiry| now >= expiry) {
                return Ok(());
            }
            if state.session.as_ref().is_some_and(|session| {
                !session.received && session.started.elapsed() >= FIRST_AUDIO_TIMEOUT
            }) {
                bail!("No microphone audio. Check the selected input device and PipeWire.");
            }
        }
        if stdout.is_none() {
            let deadline =
                *eof_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(300));
            if (stderr.is_none() || Instant::now() >= deadline)
                && let Some(status) = child.reap_exited()?
            {
                if !status.success() && status.signal() != Some(libc::SIGTERM) {
                    let detail = String::from_utf8_lossy(diagnostics.make_contiguous());
                    bail!("Microphone capture failed: {}", detail.trim());
                }
                if pending != 0 {
                    bail!("Microphone capture ended with an incomplete PCM16 sample");
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Microphone capture closed its audio stream");
            }
        }

        let mut fds = [
            libc::pollfd {
                fd: stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        poll(&mut fds, POLL_INTERVAL)?;
        if closing.load(Ordering::Acquire) {
            break;
        }
        if fds[1].revents != 0 {
            let mut message = [0; 4096];
            let count = read_available(&mut stderr, &mut message)?;
            diagnostics.extend(&message[..count]);
            let overflow = diagnostics.len().saturating_sub(DIAGNOSTIC_LIMIT);
            diagnostics.drain(..overflow);
        }
        if fds[0].revents != 0 {
            let length = pending + read_available(&mut stdout, &mut chunk[pending..CHUNK])?;
            let complete = length & !1;
            if complete != 0 {
                feed_capture(state, &chunk[..complete]);
            }
            pending = length - complete;
            if pending != 0 {
                chunk[0] = chunk[complete];
            }
        }
    }
    Ok(())
}

struct EagerInference {
    cancelled: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Transcription>>>,
}

impl Drop for EagerInference {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub struct Recorder {
    capture: Capture,
    hold: bool,
    idle_epoch: u64,
    eager: Option<EagerInference>,
}

impl Recorder {
    pub fn start(
        target: &str,
        hold: bool,
        on_frame: impl Fn(f64, f64) + Send + Sync + 'static,
        on_end: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::start_session(
            target,
            hold,
            Session::new(Arc::new(on_frame), Arc::new(on_end)),
        )
    }

    fn start_session(target: &str, hold: bool, mut session: Session) -> Result<Self> {
        let (idle, idle_epoch) = {
            let mut idle = IDLE.lock().unwrap();
            (idle.capture.take(), idle.epoch)
        };
        if let Some(capture) = idle {
            if capture.target == target {
                let mut state = capture.state.lock().unwrap();
                if !state.closed && state.expires.is_some_and(|expiry| expiry > Instant::now()) {
                    session.pcm.extend(state.ring.drain(..));
                    if let Some(spool) = &mut session.spool {
                        spool
                            .write_all(session.pcm.make_contiguous())
                            .context("Save microphone preroll for recovery")?;
                    }
                    session.received = true;
                    state.session = Some(session);
                    state.expires = None;
                    drop(state);
                    return Ok(Self {
                        capture,
                        hold,
                        idle_epoch,
                        eager: None,
                    });
                }
            }
            drop(capture);
        }
        let capture = Capture::open(target, session).context("Could not open microphone")?;
        Ok(Self {
            capture,
            hold,
            idle_epoch,
            eager: None,
        })
    }

    pub fn start_eager(
        target: &str,
        hold: bool,
        on_frame: impl Fn(f64, f64) + Send + Sync + 'static,
        on_end: impl Fn() + Send + Sync + 'static,
        engine: Arc<Engine>,
        maximum_seconds: usize,
    ) -> Result<Self> {
        Self::start_eager_spooled(
            target,
            hold,
            on_frame,
            on_end,
            engine,
            maximum_seconds,
            None,
        )
    }

    pub fn start_eager_spooled(
        target: &str,
        hold: bool,
        on_frame: impl Fn(f64, f64) + Send + Sync + 'static,
        on_end: impl Fn() + Send + Sync + 'static,
        engine: Arc<Engine>,
        maximum_seconds: usize,
        spool: Option<&Path>,
    ) -> Result<Self> {
        let epoch = engine.recording_epoch();
        // At least five seconds per piece and a 300-second recording cap bound
        // this queue below 64 entries without blocking the capture thread.
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(64);
        let mut session = Session::new(Arc::new(on_frame), Arc::new(on_end));
        session.pieces = Some((sender, maximum_seconds));
        session.spool = spool
            .map(|path| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)
                    .context("Create private recording recovery spool")
            })
            .transpose()?;
        let mut recorder = Self::start_session(target, hold, session)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            let cancelled = || interrupted.load(Ordering::Acquire);
            engine.ensure_scoped(epoch, &cancelled)?;
            let mut transcript = Transcription::default();
            let mut offset = 0.0;
            for piece in receiver {
                anyhow::ensure!(!cancelled(), "Speech inference cancelled");
                let decoded = engine.transcribe_detailed_scoped(&piece, epoch, &cancelled)?;
                anyhow::ensure!(!cancelled(), "Speech inference cancelled");
                transcript.append(decoded, offset);
                offset += piece.len() as f64 / (RATE * 2) as f64;
            }
            anyhow::ensure!(!cancelled(), "Speech inference cancelled");
            Ok(transcript)
        });
        recorder.eager = Some(EagerInference {
            cancelled,
            thread: Some(worker),
        });
        Ok(recorder)
    }

    pub fn cancel_eager(&self) {
        if let Some(eager) = &self.eager {
            eager.cancelled.store(true, Ordering::Release);
        }
    }

    pub fn stop_eager(self, after_stop: impl FnOnce()) -> Result<(Vec<u8>, String, usize)> {
        self.stop_eager_detailed(after_stop)
            .map(|(pcm, transcript, recorded)| (pcm, transcript.text, recorded))
    }

    pub fn stop_eager_detailed(
        mut self,
        after_stop: impl FnOnce(),
    ) -> Result<(Vec<u8>, Transcription, usize)> {
        let eager = self.eager.take();
        let recorded = self
            .capture
            .state
            .lock()
            .unwrap()
            .session
            .as_ref()
            .map_or(0, |session| session.recorded);
        let pcm = self.stop()?;
        after_stop();
        let text = match eager {
            Some(mut worker) => worker
                .thread
                .take()
                .expect("eager thread is present")
                .join()
                .map_err(|_| anyhow::anyhow!("Speech inference thread failed"))??,
            None => Transcription::default(),
        };
        Ok((pcm, text, recorded))
    }

    pub fn stop(self) -> Result<Vec<u8>> {
        let Self {
            mut capture,
            hold,
            idle_epoch,
            eager: _,
        } = self;
        let (session, parked, old_error) = {
            let mut state = capture.state.lock().unwrap();
            let session = state
                .session
                .take()
                .context("Microphone is not recording")?;
            if let Some(error) = &session.spool_failure {
                bail!("{error}");
            }
            if let Some(file) = &session.spool {
                file.sync_data().context("Save recording recovery spool")?;
            }
            let parked = hold && !state.closed;
            if parked {
                state.expires = Some(Instant::now() + HOLD_TIME);
            }
            (session, parked, state.failure.take())
        };
        if parked {
            let mut idle = IDLE.lock().unwrap();
            if idle.epoch == idle_epoch {
                let previous = idle.capture.replace(capture);
                drop(idle);
                drop(previous);
                return Ok(Vec::from(session.pcm));
            }
        }
        capture.close()?;
        let final_error = capture.state.lock().unwrap().failure.take();
        if let Some(error) = old_error.or(final_error) {
            bail!(error);
        }
        Ok(Vec::from(session.pcm))
    }
}

pub fn release() {
    let capture = {
        let mut idle = IDLE.lock().unwrap();
        idle.epoch = idle.epoch.wrapping_add(1);
        idle.capture.take()
    };
    drop(capture);
}
