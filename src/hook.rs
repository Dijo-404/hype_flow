use crate::{config::config_dir, process::run_command_until, vocabulary::whitespace};
use std::{fs, io, os::unix::fs::MetadataExt, path::PathBuf, process::Command, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(30);

pub fn hook_path() -> PathBuf {
    config_dir().join("transcript-hook")
}

pub fn run_hook(text: &str) -> (String, Option<String>) {
    run_hook_until(text, || false)
}

pub fn run_hook_until(text: &str, cancelled: impl Fn() -> bool) -> (String, Option<String>) {
    let fallback = |note: String| (text.to_owned(), Some(note));
    if cancelled() {
        return fallback("transcript hook cancelled".to_owned());
    }
    let path = hook_path();
    let info = match fs::metadata(&path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (text.to_owned(), None),
        Err(error) => return fallback(format!("transcript hook failed: {error}")),
    };
    // SAFETY: getuid has no arguments or preconditions.
    if !info.is_file()
        || info.uid() != unsafe { libc::getuid() }
        || info.mode() & 0o022 != 0
        || info.mode() & 0o100 == 0
    {
        return fallback(
            "transcript hook skipped: it must be your own executable, not writable by others"
                .to_owned(),
        );
    }
    let output = match run_command_until(
        &mut Command::new(path),
        Some(text.as_bytes()),
        TIMEOUT,
        true,
        cancelled,
    ) {
        Ok(output) => output,
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("timed out") {
                return fallback("transcript hook timed out after 30 s".to_owned());
            }
            if detail == "Command cancelled" {
                return fallback("transcript hook cancelled".to_owned());
            }
            return fallback(format!("transcript hook failed: {detail}"));
        }
    };
    if !output.status.success() {
        return fallback(match output.status.code() {
            Some(code) => format!("transcript hook exited with status {code}"),
            None => format!("transcript hook terminated with {}", output.status),
        });
    }
    match String::from_utf8(output.stdout) {
        Ok(transcript) => (transcript.trim_matches(whitespace).to_owned(), None),
        Err(_) => fallback("transcript hook returned invalid UTF-8".to_owned()),
    }
}
