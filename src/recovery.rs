//! Opt-in audio recovery. No audio is persisted unless enabled in settings.
use crate::backend::{PCM_LIMIT, pcm_from_wav, wav_bytes};
use anyhow::{Context, Result, ensure};
use std::{
    env,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const RETAIN: usize = 20;

fn private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path);
    match metadata {
        Ok(metadata) => ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() },
            "Unsafe recording directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn directory(kind: &str) -> Result<PathBuf> {
    ensure!(
        matches!(kind, "spool" | "failed" | "history"),
        "Unknown recording collection"
    );
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::home_dir().unwrap_or_default().join(".local/state"));
    fs::create_dir_all(&base)?;
    let root = base.join("hype-flow");
    private_directory(&root)?;
    let path = root.join(kind);
    private_directory(&path)?;
    Ok(path)
}

pub fn begin() -> Result<PathBuf> {
    let directory = directory("spool")?;
    let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = directory.join(format!("{id}-{}.pcm", std::process::id()));
    Ok(path)
}

pub fn discard(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = fs::remove_file(path);
    }
}

fn entries(directory: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == extension)
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn pcm(path: &Path) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::getuid() }
            && metadata.len() <= PCM_LIMIT as u64,
        "Unsafe or oversized recording"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(PCM_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= PCM_LIMIT && bytes.len().is_multiple_of(2),
        "Invalid recovery PCM"
    );
    Ok(bytes)
}

/// Atomically promote a completed raw spool to a bounded private WAV collection.
pub fn retain(path: &Path, kind: &str) -> Result<Option<PathBuf>> {
    let audio = pcm(path)?;
    if audio.is_empty() {
        discard(Some(path));
        return Ok(None);
    }
    let directory = directory(kind)?;
    let target = directory
        .join(path.file_stem().context("Recording has no filename")?)
        .with_extension("wav");
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let expected = wav_bytes(&audio);
    temporary.write_all(&expected)?;
    temporary.as_file().sync_all()?;
    if let Err(error) = temporary.persist_noclobber(&target) {
        if error.error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
        // A crash can leave the promoted WAV in place before the spool is removed.
        let existing = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&target)?;
        let metadata = existing.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::getuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.len() == expected.len() as u64,
            "Conflicting saved recording: {}",
            target.display()
        );
        let mut bytes = Vec::with_capacity(expected.len());
        existing
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes == expected,
            "Conflicting saved recording: {}",
            target.display()
        );
    }
    File::open(&directory)?.sync_all()?;
    fs::remove_file(path)?;
    let paths = entries(&directory, "wav")?;
    for old in paths.iter().take(paths.len().saturating_sub(RETAIN)) {
        fs::remove_file(old)?;
    }
    Ok(Some(target))
}

/// Recover interrupted sessions only when the owner has enabled audio recovery.
pub fn recover_interrupted() -> Result<usize> {
    let mut recovered = 0;
    let mut failures = 0;
    for path in entries(&directory("spool")?, "pcm")? {
        match retain(&path, "failed") {
            Ok(Some(_)) => recovered += 1,
            Ok(None) => {}
            Err(error) => {
                failures += 1;
                eprintln!("Could not recover {}: {error:#}", path.display());
            }
        }
    }
    ensure!(
        failures == 0,
        "{failures} interrupted recording(s) could not be recovered; valid recordings were preserved and unreadable spools remain private"
    );
    Ok(recovered)
}

pub fn clear() -> Result<()> {
    for kind in ["failed", "history", "spool"] {
        let path = directory(kind)?;
        for file in entries(&path, if kind == "spool" { "pcm" } else { "wav" })? {
            fs::remove_file(file)?;
        }
    }
    Ok(())
}

pub fn last(kind: &str) -> Result<Vec<u8>> {
    let path = entries(&directory(kind)?, "wav")?
        .pop()
        .context("No saved recording available")?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::getuid() },
        "Unsafe saved recording"
    );
    pcm_from_wav(file).context("Invalid saved recording WAV")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_is_private_bounded_and_ignores_symlinks() {
        // A subprocess keeps XDG changes isolated from parallel Rust tests.
        if env::var_os("HYPE_RECOVERY_TEST").is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "recovery::tests::recovery_is_private_bounded_and_ignores_symlinks",
                ])
                .env("HYPE_RECOVERY_TEST", "1")
                .env("XDG_STATE_HOME", root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        for index in 0..22 {
            let path = begin().unwrap();
            fs::write(&path, [index, 0, 1, 0]).unwrap();
            let target = retain(&path, "failed").unwrap().unwrap();
            assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
            assert!(!path.exists());
        }
        let failed = directory("failed").unwrap();
        assert_eq!(fs::metadata(&failed).unwrap().mode() & 0o777, 0o700);
        assert_eq!(entries(&failed, "wav").unwrap().len(), RETAIN);
        assert_eq!(last("failed").unwrap(), [21, 0, 1, 0]);
        let path = begin().unwrap();
        fs::write(&path, [1, 0]).unwrap();
        assert_eq!(recover_interrupted().unwrap(), 1);
        assert!(!path.exists());
        let link = directory("spool").unwrap().join("malicious.pcm");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        assert_eq!(recover_interrupted().unwrap(), 0);
        assert!(retain(&link, "failed").is_err());
        let invalid = directory("spool").unwrap().join("000-invalid.pcm");
        fs::write(&invalid, [1]).unwrap();
        let valid = begin().unwrap();
        fs::write(&valid, [42, 0]).unwrap();
        assert!(recover_interrupted().is_err());
        assert_eq!(last("failed").unwrap(), [42, 0]);
        assert!(invalid.exists());
        assert!(!valid.exists());
        // Simulate interruption after publishing the WAV and before removing the PCM.
        let spool = begin().unwrap();
        let saved = failed
            .join(spool.file_name().unwrap())
            .with_extension("wav");
        fs::write(&spool, [7, 0]).unwrap();
        fs::write(&saved, wav_bytes(&[8, 0])).unwrap();
        fs::set_permissions(&saved, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(retain(&spool, "failed").is_err());
        assert_eq!(fs::read(&spool).unwrap(), [7, 0]);
        fs::write(&saved, wav_bytes(&[7, 0])).unwrap();
        assert_eq!(retain(&spool, "failed").unwrap(), Some(saved));
        assert!(!spool.exists());
        clear().unwrap();
        assert!(last("failed").is_err());
        assert!(link.symlink_metadata().is_ok());
    }
}
