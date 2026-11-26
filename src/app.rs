use crate::{
    audio::{self, Recorder},
    backend::Engine,
    cleanup,
    config::{self, Config},
    controller::Session,
    feedback::{self, MediaPause},
    hook, output, recovery, setup, shortcuts,
    ui::{Home, Overlay},
};
use anyhow::{Context, Result};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::RefCell,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

type Transcript = (String, String, Option<String>, bool, std::time::Instant);
const MIN_RECORDING_BYTES: usize = crate::backend::RATE * 2 * 3 / 10;
const SHORT_RECORDING: &str = "Recording too short (minimum 300 ms)";

fn validate_recording_length(bytes: usize) -> Result<()> {
    anyhow::ensure!(bytes >= MIN_RECORDING_BYTES, "{SHORT_RECORDING}");
    Ok(())
}

enum Event {
    Warmed(u64, Result<()>),
    Started(
        u64,
        Result<(Recorder, Option<serde_json::Value>, Option<PathBuf>)>,
    ),
    Frame(u64, f64, f64),
    CaptureEnded(u64),
    CaptureStopped(u64),
    ShortRecording(u64),
    Transcribed(u64, Result<Transcript>),
    Delivered(u64, Result<String>),
    Downloaded(String, Result<(String, String)>),
    Saved(Box<Config>, Result<()>),
    Recovered(Result<usize>),
    Notice(String),
    Quit,
}

fn discard_pending_capture(event: Event) {
    if let Event::Started(_, Ok((recorder, _, spool))) = event {
        drop(recorder);
        recovery::discard(spool.as_deref());
    }
}

struct AppState {
    weak: Weak<RefCell<Self>>,
    application: gtk::Application,
    _hold: gio::ApplicationHoldGuard,
    values: Config,
    runtime: PathBuf,
    session: Session,
    live: Arc<AtomicU64>,
    engine: Arc<Engine>,
    recorder: Option<Recorder>,
    recording_spool: Option<PathBuf>,
    workers: Vec<thread::JoinHandle<()>>,
    stop_pending: bool,
    target: Option<serde_json::Value>,
    media: MediaPause,
    home: Option<Rc<Home>>,
    overlay: Overlay,
    status: gio::SimpleAction,
    tx: async_channel::Sender<Event>,
    rx: async_channel::Receiver<Event>,
    notch: Option<gio::DBusActionGroup>,
    notch_watch: Option<gio::WatcherId>,
    last: (&'static str, String, bool, f64, f64),
    unloading: Option<glib::SourceId>,
    settling: Option<glib::SourceId>,
    downloading: bool,
    recovering: bool,
    signals: Option<signal_hook::iterator::Handle>,
}

fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Settings the speech worker reads at startup; changing one restarts it.
fn worker_changed(old: &Config, new: &Config) -> bool {
    old.model != new.model
        || old.server != new.server
        || old.language != new.language
        || old.threads != new.threads
        || old.blank_penalty != new.blank_penalty
        || old.max_active_paths != new.max_active_paths
        || old.vad_threshold != new.vad_threshold
        || old.vad_min_silence != new.vad_min_silence
        || old.max_piece_seconds != new.max_piece_seconds
        || old.vocabulary_boosting != new.vocabulary_boosting
        || new.vocabulary_boosting
            && (old.prompt != new.prompt || old.boost_score != new.boost_score)
        || old.cut_silence != new.cut_silence
        || old.noise_suppression != new.noise_suppression
        || old.normalize_gain != new.normalize_gain
        || old.feature_dim != new.feature_dim
        || old.provider != new.provider
        || old.soft_speech_retry != new.soft_speech_retry
}

fn finish_recognition(
    transcript: crate::backend::Transcription,
    values: &Config,
    cancelled: impl Fn() -> bool,
) -> Result<(String, String, Option<String>)> {
    anyhow::ensure!(!cancelled(), "Speech inference cancelled");
    if transcript.text.is_empty() {
        return Ok((String::new(), String::new(), None));
    }
    if values.hallucination_filtering && transcript.anomaly_score >= values.anomaly_threshold {
        return Ok((
            transcript.text,
            String::new(),
            Some("Anomalous recognition filtered; original saved".into()),
        ));
    }
    let cleaned = cleanup::process_with_confidence(&transcript.text, values, &transcript.words)?;
    let (text, note) = hook::run_hook_until(&cleaned, cancelled);
    Ok((transcript.text, text, note))
}

impl AppState {
    fn show(&mut self) {
        if self.home.is_none() {
            let application = self.application.downgrade();
            let actions: Rc<dyn Fn(&str)> = Rc::new(move |name| {
                if let Some(application) = application.upgrade() {
                    application.activate_action(name, None);
                }
            });
            let weak = self.weak.clone();
            let save: Rc<dyn Fn(Config) -> bool> = Rc::new(move |values| {
                weak.upgrade()
                    .is_some_and(|app| app.borrow_mut().save(values))
            });
            let home = Home::new(&self.application, &self.values, actions, save);
            home.update(self.last.0, &self.last.1, setup::assets_ready(&self.values));
            self.home = Some(home);
        }
        if let Some(home) = &self.home {
            home.window.present();
        }
    }

    fn publish(
        &mut self,
        phase: &'static str,
        message: &str,
        visible: bool,
        volume: f64,
        duration: f64,
    ) {
        self.session.phase = phase;
        let changed = self.last.0 != phase || self.last.1 != message;
        self.last = (phase, message.to_string(), visible, volume, duration);
        self.status
            .set_state(&(phase, message, volume, duration, visible).to_variant());
        let notch = self
            .notch
            .as_ref()
            .is_some_and(|group| group.has_action("dictation"));
        self.overlay
            .update(phase, message, visible && !notch, volume, duration);
        if changed && let Some(home) = &self.home {
            home.update(phase, message, setup::assets_ready(&self.values));
        }
        if !matches!(phase, "idle" | "done" | "error") {
            if let Some(timer) = self.unloading.take() {
                timer.remove();
            }
        } else if self.unloading.is_none()
            && self.values.unload_minutes > 0
            && self.session.ready
            && !self.session.unloaded
        {
            let weak = self.weak.clone();
            self.unloading = Some(glib::timeout_add_local_once(
                if self.values.unload_minutes == config::UNLOAD_AFTER_EACH {
                    Duration::ZERO
                } else {
                    Duration::from_secs(u64::from(self.values.unload_minutes) * 60)
                },
                move || {
                    if let Some(app) = weak.upgrade() {
                        let mut app = app.borrow_mut();
                        app.unloading.take();
                        if matches!(app.session.phase, "idle" | "done" | "error")
                            && !app.session.busy
                            && app.recorder.is_none()
                        {
                            let reap = app.engine.stop_in_background();
                            app.workers.push(reap);
                            app.session.unloaded = true;
                        }
                    }
                },
            ));
        }
        if phase == "done" {
            if let Some(timer) = self.settling.take() {
                timer.remove();
            }
            let weak = self.weak.clone();
            let generation = self.session.generation;
            self.settling = Some(glib::timeout_add_local_once(
                Duration::from_millis(1800),
                move || {
                    if let Some(app) = weak.upgrade() {
                        let mut app = app.borrow_mut();
                        app.settling.take();
                        if app.session.current(generation) && app.session.phase == "done" {
                            app.publish("idle", "Ready", false, 0.0, 0.0);
                        }
                    }
                },
            ));
        }
    }

    fn warm(&mut self) {
        self.engine.prepare();
        self.session.unloaded = false;
        self.session.busy = true;
        self.publish("loading", "Loading speech model", false, 0.0, 0.0);
        let tx = self.tx.clone();
        let engine = self.engine.clone();
        let generation = self.session.generation;
        self.workers.push(thread::spawn(move || {
            let result = engine.ensure();
            let _ = tx.send_blocking(Event::Warmed(generation, result));
        }));
    }

    fn command(&mut self, name: &str) {
        self.workers.retain(|worker| !worker.is_finished());
        match name {
            "show" => self.show(),
            "quit" => self.application.quit(),
            "copy-last" | "copy-raw" => {
                let path = self.runtime.join(if name == "copy-raw" {
                    "last-raw-transcript.txt"
                } else {
                    "last-transcript.txt"
                });
                let raw = name == "copy-raw";
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let result=fs::read_to_string(path).context("No transcript in this session yet").and_then(|text|{
                        anyhow::ensure!(!text.is_empty(),"All words were filtered. Use Copy original to recover the unedited transcript.");
                        output::copy_text(&text,false)?;Ok(if raw {"Copied original transcript"}else{"Copied last transcript"})
                    });
                    let message = match result {
                        Ok(text) => text.to_string(),
                        Err(error) => format!("{error:#}"),
                    };
                    let _ = tx.send_blocking(Event::Notice(message));
                });
            }
            "retry-last" => self.retry("failed"),
            "retry-history" => self.retry("history"),
            "clear-recordings" => {
                if !self.recovering
                    && !self.session.busy
                    && !matches!(self.session.phase, "starting" | "recording")
                {
                    let message = match recovery::clear() {
                        Ok(()) => "Saved recordings deleted".into(),
                        Err(error) => format!("Could not delete recordings: {error:#}"),
                    };
                    self.publish("idle", &message, false, 0.0, 0.0);
                }
            }
            "press" => {
                if self.downloading || !setup::assets_ready(&self.values) {
                    self.show();
                    return;
                }
                if !matches!(self.session.phase, "starting" | "recording") && !self.session.busy {
                    if self.session.ready {
                        self.start();
                    } else {
                        self.warm();
                    }
                }
            }
            "release" => {
                if matches!(self.session.phase, "starting" | "recording") {
                    self.stop();
                }
            }
            "download" => self.download(),
            "shortcuts" => {
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let message = match shortcuts::install() {
                        Ok(message) => message,
                        Err(error) => format!("{error:#}"),
                    };
                    let _ = tx.send_blocking(Event::Notice(message));
                });
            }
            "cancel" => self.cancel(),
            "toggle" => {
                if !setup::assets_ready(&self.values) || self.downloading {
                    self.show();
                    return;
                }
                if matches!(self.session.phase, "starting" | "recording") {
                    self.stop();
                } else if self.session.busy {
                    // Busy while idle is a shortcut save, which has no progress to show.
                    if self.session.phase != "idle" {
                        let last = self.last.clone();
                        self.publish(last.0, &last.1, true, last.3, last.4);
                    }
                } else if !self.session.ready {
                    self.warm();
                } else {
                    self.start();
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) {
        if self.recovering {
            if let Some(home) = &self.home {
                home.feedback("Finish recovering interrupted recordings before starting dictation");
            }
            return;
        }
        let Some(generation) = self.session.start() else {
            return;
        };
        self.live.store(generation, Ordering::Release);
        self.stop_pending = false;
        self.engine.prepare();
        self.publish("starting", "Opening microphone", true, 0.0, 0.0);
        if self.values.pause_media {
            self.media.pause();
        }
        // The eager capture thread loads the worker as soon as the microphone opens.
        self.session.unloaded = false;
        let tx = self.tx.clone();
        let target = self.values.input_target.clone();
        let hold = self.values.keep_mic_ready;
        let engine = self.engine.clone();
        let maximum_seconds = self.values.max_piece_seconds as usize;
        let recover_audio = self.values.recover_recordings || self.values.history_enabled;
        self.workers.push(thread::spawn(move || {
            let frames = tx.clone();
            let ends = tx.clone();
            let spool = if recover_audio {
                recovery::begin().map(Some)
            } else {
                Ok(None)
            };
            let result = spool.and_then(|spool| {
                let result = Recorder::start_eager_spooled(
                    &target,
                    hold,
                    move |level, duration| {
                        let _ = frames.try_send(Event::Frame(generation, level, duration));
                    },
                    move || {
                        let _ = ends.send_blocking(Event::CaptureEnded(generation));
                    },
                    engine,
                    maximum_seconds,
                    spool.as_deref(),
                )
                .map(|recorder| {
                    let target = output::focused_window();
                    (recorder, target, spool.clone())
                });
                if result.is_err() {
                    recovery::discard(spool.as_deref());
                }
                result
            });
            if let Err(error) = tx.send_blocking(Event::Started(generation, result)) {
                discard_pending_capture(error.into_inner());
            }
        }));
    }

    fn stop(&mut self) {
        let Some(recorder) = self.recorder.take() else {
            self.stop_pending = true;
            return;
        };
        let Some(generation) = self.session.stop() else {
            return;
        };
        self.publish("transcribing", "Transcribing", true, 0.0, 0.0);
        let engine = self.engine.clone();
        let values = self.values.clone();
        let live = self.live.clone();
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        let recording_epoch = engine.recording_epoch();
        let spool = self.recording_spool.take();
        let stopped = std::time::Instant::now();
        self.workers.push(thread::spawn(move || {
            let cancelled = || generation != live.load(Ordering::Acquire);
            let mut recording_too_short = false;
            let result = (|| {
                let (pcm, mut committed, recorded) = recorder.stop_eager_detailed(|| {
                    if generation != live.load(Ordering::Acquire) {
                        return;
                    }
                    let _ = tx.send_blocking(Event::CaptureStopped(generation));
                    if values.sounds && generation == live.load(Ordering::Acquire) {
                        let _ = feedback::play_cue("stop", &runtime);
                    }
                })?;
                recording_too_short = recorded < MIN_RECORDING_BYTES;
                validate_recording_length(recorded)?;
                anyhow::ensure!(
                    generation == live.load(Ordering::Acquire),
                    "Speech inference cancelled"
                );
                engine.ensure_scoped(recording_epoch, &cancelled)?;
                let tail = engine.transcribe_detailed_scoped(&pcm, recording_epoch, &cancelled)?;
                let tail_offset = committed.audio_seconds;
                committed.append(tail, tail_offset);
                let silent = committed.text.is_empty() && !pcm.iter().any(|byte| *byte != 0);
                let (raw, text, note) = finish_recognition(committed, &values, cancelled)?;
                Ok((raw, text, note, silent, stopped))
            })();
            if let Some(path) = spool.as_deref() {
                if cancelled() || recording_too_short {
                    recovery::discard(Some(path));
                } else if result.is_err() && values.recover_recordings {
                    if let Err(error) = recovery::retain(path, "failed") {
                        eprintln!("Could not retain failed recording: {error:#}");
                    }
                } else if result.is_ok() && values.history_enabled {
                    if let Err(error) = recovery::retain(path, "history") {
                        eprintln!("Could not retain recording history: {error:#}");
                    }
                } else {
                    recovery::discard(Some(path));
                }
            }
            let event = if recording_too_short {
                Event::ShortRecording(generation)
            } else {
                Event::Transcribed(generation, result)
            };
            let _ = tx.send_blocking(event);
        }));
    }

    fn cancel(&mut self) {
        if !self.session.cancel() {
            // Nothing to abort: dismiss the capsule and keep a warm worker loaded.
            let last = self.last.clone();
            self.publish(last.0, &last.1, false, last.3, last.4);
            return;
        }
        self.live.store(self.session.generation, Ordering::Release);
        self.stop_pending = false;
        self.media.resume();
        if let Some(recorder) = self.recorder.take() {
            recorder.cancel_eager();
            let spool = self.recording_spool.take();
            self.workers.push(thread::spawn(move || {
                drop(recorder);
                recovery::discard(spool.as_deref());
            }));
        } else {
            // A pending opener or the stop thread owns the eager decoder; only a new epoch aborts it.
            self.session.unloaded = true;
            self.workers.push(self.engine.stop_in_background());
        }
        self.session.busy = false;
        self.publish("idle", "Cancelled", false, 0.0, 0.0);
    }

    fn save(&mut self, values: Config) -> bool {
        if self.recovering
            || self.downloading
            || self.session.busy
            || matches!(self.session.phase, "starting" | "recording")
        {
            if let Some(home) = &self.home {
                home.feedback("Finish recovery, download or dictation before changing settings");
            }
            return false;
        }
        let shortcuts_changed = values.dictation_shortcut != self.values.dictation_shortcut
            || values.cancel_shortcut != self.values.cancel_shortcut
            || values.dictation_mode != self.values.dictation_mode;
        if shortcuts_changed {
            // niri validation can take seconds; busy blocks dictation and saves until Saved.
            self.session.busy = true;
            let tx = self.tx.clone();
            self.workers.push(thread::spawn(move || {
                let saved = shortcuts::save(&values).map(|_| ());
                let _ = tx.send_blocking(Event::Saved(Box::new(values), saved));
            }));
            return false;
        }
        let saved = values.save();
        self.apply(values, saved, false)
    }

    fn apply(&mut self, values: Config, saved: Result<()>, shortcuts_changed: bool) -> bool {
        if let Err(error) = saved {
            if let Some(home) = &self.home {
                home.feedback(&format!("{error:#}"));
            }
            return false;
        }
        let reload = worker_changed(&self.values, &values);
        self.values = values;
        if let Some(timer) = self.unloading.take() {
            timer.remove();
        }
        if !self.values.keep_mic_ready {
            audio::release();
        }
        let mut last = self.last.clone();
        if reload || !self.session.ready {
            self.session.generation += 1;
            self.live.store(self.session.generation, Ordering::Release);
            self.workers.push(self.engine.stop_in_background());
            self.engine = Arc::new(Engine::new(self.values.clone(), self.runtime.clone()));
            if setup::assets_ready(&self.values) {
                // Load lazily like startup; a bad setting surfaces on the next dictation.
                self.session.ready = true;
                self.session.unloaded = true;
                last = ("idle", "Ready".into(), false, 0.0, 0.0);
            }
        }
        if let Some(home) = &self.home {
            home.feedback("Settings saved");
        }
        let message = if shortcuts_changed {
            format!(
                "Shortcuts saved. {} starts dictation.",
                self.values.dictation_shortcut
            )
        } else {
            last.1
        };
        self.publish(last.0, &message, last.2, last.3, last.4);
        true
    }

    fn retry(&mut self, kind: &'static str) {
        if self.recovering
            || self.downloading
            || self.session.busy
            || matches!(self.session.phase, "starting" | "recording")
        {
            return;
        }
        if kind == "failed" && !self.values.recover_recordings
            || kind == "history" && !self.values.history_enabled
        {
            self.publish(
                "error",
                "Enable recording recovery or history before retrying audio",
                true,
                0.0,
                0.0,
            );
            return;
        }
        self.session.generation += 1;
        let generation = self.session.generation;
        self.live.store(generation, Ordering::Release);
        self.engine.prepare();
        let epoch = self.engine.recording_epoch();
        self.session.busy = true;
        self.session.unloaded = false;
        // A recovered recording has no trustworthy focused target; copy its text.
        self.target = None;
        self.publish("transcribing", "Retrying saved recording", true, 0.0, 0.0);
        let values = self.values.clone();
        let engine = self.engine.clone();
        let live = self.live.clone();
        let tx = self.tx.clone();
        self.workers.push(thread::spawn(move || {
            let cancelled = || generation != live.load(Ordering::Acquire);
            let result = (|| {
                let pcm = recovery::last(kind)?;
                validate_recording_length(pcm.len())?;
                engine.ensure_scoped(epoch, &cancelled)?;
                let transcript = engine.transcribe_detailed_scoped(&pcm, epoch, &cancelled)?;
                let (raw, text, note) = finish_recognition(transcript, &values, cancelled)?;
                Ok((raw, text, note, false, std::time::Instant::now()))
            })();
            let _ = tx.send_blocking(Event::Transcribed(generation, result));
        }));
    }

    fn download(&mut self) {
        if self.downloading
            || self.session.busy
            || matches!(self.session.phase, "starting" | "recording")
        {
            return;
        }
        self.downloading = true;
        if let Some(home) = &self.home {
            home.set_downloading(true);
        }
        let tx = self.tx.clone();
        let variant = self.values.model_variant.clone();
        thread::spawn(move || {
            let progress = tx.clone();
            let result = setup::install_variant(&variant, move |message| {
                let _ = progress.send_blocking(Event::Notice(message.to_string()));
            });
            let _ = tx.send_blocking(Event::Downloaded(variant, result));
        });
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Warmed(generation, result) if self.session.current(generation) => {
                self.session.busy = false;
                self.session.ready = result.is_ok();
                match result {
                    Ok(()) => self.publish("idle", "Ready", false, 0.0, 0.0),
                    Err(error) => {
                        self.workers.push(self.engine.stop_in_background());
                        self.publish("error", &format!("{error:#}"), true, 0.0, 0.0);
                    }
                }
            }
            Event::Started(generation, result) => {
                if !self.session.current(generation) {
                    self.workers.push(thread::spawn(move || {
                        if let Ok((recorder, _, spool)) = result {
                            drop(recorder);
                            recovery::discard(spool.as_deref());
                        }
                    }));
                    return;
                }
                match result {
                    Ok((recorder, target, spool)) => {
                        self.recording_spool = spool;
                        self.recorder = Some(recorder);
                        self.target = target;
                        if self.stop_pending {
                            self.stop_pending = false;
                            self.stop();
                        }
                    }
                    Err(error) => {
                        self.media.resume();
                        self.publish("error", &format!("{error:#}"), true, 0.0, 0.0);
                    }
                }
            }
            Event::Frame(generation, volume, duration) => {
                let first = self.session.phase == "starting";
                if self.session.frame(generation) {
                    if first && self.values.sounds {
                        let _ = feedback::play_cue("start", &self.runtime);
                    }
                    self.publish("recording", "Listening", true, volume, duration);
                }
            }
            Event::CaptureEnded(generation)
                if self.session.current(generation)
                    && matches!(self.session.phase, "starting" | "recording") =>
            {
                self.stop()
            }
            Event::CaptureStopped(generation) if self.session.current(generation) => {
                self.media.resume();
            }
            Event::ShortRecording(generation) if self.session.current(generation) => {
                self.media.resume();
                self.session.busy = false;
                self.publish("done", SHORT_RECORDING, true, 0.0, 0.0);
            }
            Event::Transcribed(generation, result) if self.session.current(generation) => {
                self.media.resume();
                self.session.busy = false;
                match result {
                    Err(error) => {
                        self.session.fail();
                        self.workers.push(self.engine.stop_in_background());
                        self.publish("error", &format!("{error:#}"), true, 0.0, 0.0);
                    }
                    Ok((raw, text, note, silent, stopped)) => {
                        self.session.ready = true;
                        if raw.is_empty() {
                            self.publish(
                                "done",
                                if silent {
                                    "Microphone is silent · check it is not muted"
                                } else {
                                    "No speech detected"
                                },
                                true,
                                0.0,
                                0.0,
                            );
                            return;
                        }
                        if let Err(error) =
                            write_private(&self.runtime.join("last-raw-transcript.txt"), &raw)
                                .and_then(|()| {
                                    write_private(&self.runtime.join("last-transcript.txt"), &text)
                                })
                        {
                            self.publish(
                                "error",
                                &format!("Could not save transcript for recovery. {error:#}"),
                                true,
                                0.0,
                                0.0,
                            );
                            return;
                        }
                        if text.is_empty() {
                            self.publish(
                                "done",
                                note.as_deref()
                                    .unwrap_or("All words filtered · original saved"),
                                true,
                                0.0,
                                0.0,
                            );
                            return;
                        }
                        self.session.busy = true;
                        self.publish("delivering", "Copying transcript", true, 0.0, 0.0);
                        let values = self.values.clone();
                        let target = self.target.clone();
                        let tx = self.tx.clone();
                        let live = self.live.clone();
                        self.workers.push(thread::spawn(move || {
                            let result = output::deliver_guarded_until(
                                &text,
                                target.as_ref(),
                                values.paste,
                                values.trailing_space,
                                values.press_enter,
                                values.modifier_release_guard,
                                stopped,
                                || generation != live.load(Ordering::Acquire),
                            )
                            .map(|message| {
                                if let Some(note) = note {
                                    format!("{message} · {note}")
                                } else {
                                    message
                                }
                            });
                            let _ = tx.send_blocking(Event::Delivered(generation, result));
                        }));
                    }
                }
            }
            Event::Delivered(generation, result) if self.session.current(generation) => {
                self.session.busy = false;
                match result {
                    Ok(message) => self.publish("done", &message, true, 0.0, 0.0),
                    Err(error) => self.publish(
                        "error",
                        &format!("Paste failed. Use Copy transcript in Hype Flow. {error:#}"),
                        true,
                        0.0,
                        0.0,
                    ),
                }
            }
            Event::Saved(values, saved) => {
                self.session.busy = false;
                if self.apply(*values, saved, true)
                    && let Some(home) = &self.home
                {
                    home.saved(self.values.clone());
                }
            }
            Event::Downloaded(variant, result) => {
                self.downloading = false;
                if let Some(home) = &self.home {
                    home.set_downloading(false);
                }
                match result {
                    Ok((model, server)) => {
                        let mut values = self.values.clone();
                        let spec = match setup::model_spec(&variant) {
                            Ok(spec) => spec,
                            Err(error) => {
                                self.publish("error", &format!("{error:#}"), false, 0.0, 0.0);
                                return;
                            }
                        };
                        values.model_variant = variant;
                        values.feature_dim = spec.feature_dim as i32;
                        values.provider = "cpu".into();
                        if !spec.boosting {
                            values.vocabulary_boosting = false;
                        }
                        values.model = model.clone();
                        values.server = server;
                        values.language = "en".into();
                        if self.save(values)
                            && let Some(home) = &self.home
                        {
                            home.saved(self.values.clone());
                        }
                        // Load the installed model now so a broken install fails here, not after a dictation.
                        if self.values.model == model {
                            self.warm();
                        }
                    }
                    Err(error) => self.publish("error", &format!("{error:#}"), false, 0.0, 0.0),
                }
            }
            Event::Recovered(result) => {
                self.recovering = false;
                match result {
                    Ok(0) => {}
                    Ok(count) => {
                        self.show();
                        if let Some(home) = &self.home {
                            home.feedback(&format!(
                                "Recovered {count} interrupted recording(s). Use Retry last failed."
                            ));
                        }
                    }
                    Err(error) => {
                        self.show();
                        if let Some(home) = &self.home {
                            home.feedback(&format!("Recording recovery failed: {error:#}"));
                        }
                    }
                }
            }
            Event::Notice(message) => {
                if let Some(home) = &self.home {
                    home.feedback(&message);
                }
            }
            Event::Quit => self.application.quit(),
            _ => {}
        }
    }

    fn close(&mut self) {
        self.session.closing = true;
        self.session.generation += 1;
        self.live.store(self.session.generation, Ordering::Release);
        self.tx.close();
        self.media.resume();
        self.engine.stop();
        self.recorder.take();
        recovery::discard(self.recording_spool.take().as_deref());
        audio::release();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        while let Ok(event) = self.rx.try_recv() {
            discard_pending_capture(event);
        }
        output::finish_pending();
        if let Some(handle) = self.signals.take() {
            handle.close();
        }
        if let Some(timer) = self.unloading.take() {
            timer.remove();
        }
        if let Some(timer) = self.settling.take() {
            timer.remove();
        }
        if let Some(watch) = self.notch_watch.take() {
            gio::bus_unwatch_name(watch);
        }
        self.overlay.window.close();
        if let Some(home) = &self.home {
            home.window.destroy();
        }
        for name in ["last-transcript.txt", "last-raw-transcript.txt"] {
            let _ = fs::remove_file(self.runtime.join(name));
        }
    }
}

pub fn run(values: Config) -> Result<glib::ExitCode> {
    let runtime = config::runtime_dir()?;
    let application = gtk::Application::builder()
        .application_id("dev.hype.Flow")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    for name in ["background", "toggle", "cancel", "press", "release"] {
        application.add_main_option(
            name,
            glib::Char::from(0),
            glib::OptionFlags::NONE,
            glib::OptionArg::None,
            "Desktop integration",
            None,
        );
    }
    let shared: Rc<RefCell<Option<Rc<RefCell<AppState>>>>> = Rc::new(RefCell::new(None));
    let startup = shared.clone();
    let startup_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let errors = startup_error.clone();
    application.connect_startup(move |application| {
        let status = gio::SimpleAction::new_stateful(
            "status",
            None,
            &("idle", "Ready", 0.0f64, 0.0f64, false).to_variant(),
        );
        application.add_action(&status);
        let (tx, rx) = async_channel::bounded(128);
        let actions: Rc<dyn Fn(&str)> = {
            let application = application.downgrade();
            Rc::new(move |name| {
                if let Some(application) = application.upgrade() {
                    application.activate_action(name, None);
                }
            })
        };
        let overlay = match Overlay::new(actions.clone()) {
            Ok(overlay) => overlay,
            Err(error) => {
                *errors.borrow_mut() = Some(format!("{error:#}"));
                application.quit();
                return;
            }
        };
        let app = Rc::new_cyclic(|weak| {
            RefCell::new(AppState {
                weak: weak.clone(),
                application: application.clone(),
                _hold: application.hold(),
                values: values.clone(),
                runtime: runtime.clone(),
                session: Session::default(),
                live: Arc::new(AtomicU64::new(0)),
                engine: Arc::new(Engine::new(values.clone(), runtime.clone())),
                recorder: None,
                recording_spool: None,
                workers: Vec::new(),
                stop_pending: false,
                target: None,
                media: MediaPause::default(),
                home: None,
                overlay,
                status,
                tx: tx.clone(),
                rx: rx.clone(),
                notch: None,
                notch_watch: None,
                last: ("loading", String::new(), false, 0.0, 0.0),
                unloading: None,
                settling: None,
                downloading: false,
                recovering: false,
                signals: None,
            })
        });
        for name in [
            "toggle",
            "cancel",
            "show",
            "copy-last",
            "copy-raw",
            "retry-last",
            "retry-history",
            "clear-recordings",
            "press",
            "release",
            "quit",
            "download",
            "shortcuts",
        ] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&app);
            action.connect_activate(move |action, _| {
                if let Some(app) = weak.upgrade() {
                    app.borrow_mut().command(action.name().as_str());
                }
            });
            application.add_action(&action);
        }
        let weak = Rc::downgrade(&app);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(app) = weak.upgrade() else {
                    break;
                };
                app.borrow_mut().handle(event);
            }
        });
        if let Some(connection) = application.dbus_connection() {
            let appeared = Rc::downgrade(&app);
            let vanished = appeared.clone();
            let watch = gio::bus_watch_name_on_connection(
                &connection,
                "dev.hype_niri.Notch",
                gio::BusNameWatcherFlags::NONE,
                move |connection, name, _| {
                    let group =
                        gio::DBusActionGroup::get(&connection, Some(name), "/dev/hype_niri/Notch");
                    for removed in [false, true] {
                        let weak = appeared.clone();
                        let changed = move |_: &gio::DBusActionGroup, _: &str| {
                            if let Some(app) = weak.upgrade() {
                                let mut app = app.borrow_mut();
                                let last = app.last.clone();
                                app.publish(last.0, &last.1, last.2, last.3, last.4);
                            }
                        };
                        if removed {
                            group.connect_action_removed(None, changed);
                        } else {
                            group.connect_action_added(None, changed);
                        }
                    }
                    if let Some(app) = appeared.upgrade() {
                        let mut app = app.borrow_mut();
                        app.notch = Some(group);
                        let last = app.last.clone();
                        app.publish(last.0, &last.1, last.2, last.3, last.4);
                    }
                },
                move |_, _| {
                    if let Some(app) = vanished.upgrade() {
                        let mut app = app.borrow_mut();
                        app.notch = None;
                        let last = app.last.clone();
                        app.publish(last.0, &last.1, last.2, last.3, last.4);
                    }
                },
            );
            app.borrow_mut().notch_watch = Some(watch);
        }
        if let Ok(mut signals) = signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT])
        {
            app.borrow_mut().signals = Some(signals.handle());
            let tx = tx.clone();
            thread::spawn(move || {
                if signals.forever().next().is_some() {
                    let _ = tx.send_blocking(Event::Quit);
                }
            });
        }
        if values.recover_recordings || values.history_enabled {
            let tx = tx.clone();
            app.borrow_mut().recovering = true;
            let recovery = thread::spawn(move || {
                let result = recovery::recover_interrupted();
                let _ = tx.send_blocking(Event::Recovered(result));
            });
            app.borrow_mut().workers.push(recovery);
        }
        if setup::assets_ready(&values) {
            let mut app = app.borrow_mut();
            app.session.ready = true;
            app.session.unloaded = true;
            app.publish("idle", "Ready", false, 0.0, 0.0);
        } else {
            app.borrow_mut().publish(
                "idle",
                "Download the speech model to get started",
                false,
                0.0,
                0.0,
            );
        }
        *startup.borrow_mut() = Some(app);
    });
    let commands = shared.clone();
    application.connect_command_line(move |_, command| {
        if let Some(app) = commands.borrow().as_ref() {
            let options = command.options_dict();
            let mut app = app.borrow_mut();
            if options.contains("toggle") {
                app.command("toggle");
            } else if options.contains("cancel") {
                app.command("cancel");
            } else if options.contains("press") {
                app.command("press");
            } else if options.contains("release") {
                app.command("release");
            } else if !options.contains("background") {
                app.command("show");
            }
        }
        glib::ExitCode::SUCCESS
    });
    let shutdown = shared.clone();
    application.connect_shutdown(move |_| {
        if let Some(app) = shutdown.borrow_mut().take() {
            app.borrow_mut().close();
        }
    });
    let code = application.run();
    if let Some(error) = startup_error.borrow_mut().take() {
        anyhow::bail!(error);
    }
    Ok(code)
}

#[cfg(test)]
mod finish_tests {
    use super::*;
    #[test]
    fn live_and_retried_recordings_require_three_hundred_milliseconds() {
        assert!(validate_recording_length(0).is_err());
        assert!(validate_recording_length(MIN_RECORDING_BYTES - 2).is_err());
        assert!(validate_recording_length(MIN_RECORDING_BYTES).is_ok());
    }

    #[test]
    fn anomaly_filter_preserves_original_and_cancellation_wins() {
        let values = Config {
            hallucination_filtering: true,
            ..Config::default()
        };
        let transcript = crate::backend::Transcription {
            text: "original words".into(),
            anomaly_score: 4.0,
            ..Default::default()
        };
        let (raw, cleaned, notice) =
            finish_recognition(transcript.clone(), &values, || false).unwrap();
        assert_eq!(raw, "original words");
        assert!(cleaned.is_empty());
        assert!(notice.unwrap().contains("original saved"));
        assert!(finish_recognition(transcript, &values, || true).is_err());
    }

    #[test]
    fn only_settings_the_worker_reads_restart_it() {
        let old = Config {
            vocabulary_boosting: false,
            ..Config::default()
        };
        let app_only = Config {
            hallucination_filtering: !old.hallucination_filtering,
            anomaly_threshold: old.anomaly_threshold + 1.0,
            prompt: "Hype Flow".into(),
            boost_score: old.boost_score + 1.0,
            ..old.clone()
        };
        assert!(!worker_changed(&old, &app_only));
        let boosted = Config {
            vocabulary_boosting: true,
            ..old.clone()
        };
        assert!(worker_changed(&old, &boosted));
        for changed in [
            Config {
                prompt: "Hype Flow".into(),
                ..boosted.clone()
            },
            Config {
                boost_score: boosted.boost_score + 1.0,
                ..boosted.clone()
            },
        ] {
            assert!(worker_changed(&boosted, &changed));
        }
    }
}
