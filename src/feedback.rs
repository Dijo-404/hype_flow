use crate::process::run_command;
use anyhow::{Result, bail};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::{
    f64::consts::TAU,
    path::Path,
    process::Command,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::NamedTempFile;

const RATE: u32 = 48_000;
const NOTE: u32 = 4320;
const ATTACK: u32 = 720;
const GAP: u32 = 1200;

fn write_cue(path: &Path, frequencies: [f64; 2]) -> Result<()> {
    let file = NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let mut writer = WavWriter::new(
        file.reopen()?,
        WavSpec {
            channels: 1,
            sample_rate: RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )?;
    for frequency in frequencies {
        for index in 0..NOTE {
            let index = f64::from(index);
            // A short attack and exponential release avoid clicks at the note edges.
            let envelope =
                (index / f64::from(ATTACK)).min(1.0) * (-4.0 * index / f64::from(NOTE)).exp();
            let sample =
                32767.0 * 0.2 * envelope * (TAU * frequency * index / f64::from(RATE)).sin();
            writer.write_sample(sample as i16)?;
        }
        for _ in 0..GAP {
            writer.write_sample(0i16)?;
        }
    }
    writer.finalize()?;
    file.persist(path)?;
    Ok(())
}

pub fn play_cue(name: &str, runtime: &Path) -> Result<()> {
    let frequencies = match name {
        "start" => [523.25, 659.25],
        "stop" => [587.33, 440.0],
        _ => bail!("Unknown dictation cue"),
    };
    let path = runtime.join(format!("cue-{name}.wav"));
    thread::Builder::new().spawn(move || {
        if !path.exists() && write_cue(&path, frequencies).is_err() {
            return;
        }
        let _ = run_command(
            Command::new("pw-play")
                .args(["--media-role", "Notification"])
                .arg(path),
            None,
            Duration::from_secs(3),
        );
    })?;
    Ok(())
}

enum MediaAction {
    Pause,
    Resume,
}

#[derive(Default)]
pub struct MediaPause {
    worker: Option<(Sender<MediaAction>, JoinHandle<()>)>,
}

impl MediaPause {
    pub fn pause(&mut self) {
        if self.worker.is_none() {
            let (sender, receiver) = mpsc::channel();
            if let Ok(worker) = thread::Builder::new().spawn(move || {
                let mut paused = Vec::new();
                for action in receiver {
                    match action {
                        MediaAction::Pause => pause_playing(&mut paused),
                        MediaAction::Resume => resume_paused(&mut paused),
                    }
                }
                resume_paused(&mut paused);
            }) {
                self.worker = Some((sender, worker));
            }
        }
        if let Some((sender, _)) = &self.worker {
            let _ = sender.send(MediaAction::Pause);
        }
    }

    pub fn resume(&mut self) {
        if let Some((sender, _)) = &self.worker {
            let _ = sender.send(MediaAction::Resume);
        }
    }
}

impl Drop for MediaPause {
    fn drop(&mut self) {
        if let Some((sender, worker)) = self.worker.take() {
            drop(sender);
            let _ = worker.join();
        }
    }
}

fn playerctl(args: &[&str]) -> Option<String> {
    let output = run_command(
        Command::new("playerctl").args(args),
        None,
        Duration::from_secs(2),
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn pause_playing(paused: &mut Vec<String>) {
    // shortcut: one hung player times out this query and nothing is paused, query players one by one if that bites.
    let Ok(output) = run_command(
        Command::new("playerctl").args([
            "--all-players",
            "--format",
            "{{playerInstance}}\t{{status}}",
            "status",
        ]),
        None,
        Duration::from_secs(2),
    ) else {
        return;
    };
    // Ignore the exit status so one failing player does not hide those already listed.
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((player, "Playing")) = line.split_once('\t')
            && !paused.iter().any(|name| name == player)
            && playerctl(&["--player", player, "pause"]).is_some()
        {
            paused.push(player.to_owned());
        }
    }
}

fn resume_paused(paused: &mut Vec<String>) {
    for player in paused.drain(..) {
        let _ = playerctl(&["--player", &player, "play"]);
    }
}
