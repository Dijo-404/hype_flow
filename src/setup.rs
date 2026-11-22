use crate::{
    config::{Config, data_dir},
    process::run_command,
    vocabulary::sentencepiece_vocabulary,
};
use anyhow::{Context, Result, bail, ensure};
use bzip2::read::BzDecoder;
use reqwest::{StatusCode, blocking::Client, header};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tar::Archive;
use tempfile::{Builder, NamedTempFile, TempDir};

const RELEASE: &str = "1.13.8";
const RUNTIME_NAME: &str = "sherpa-onnx-v1.13.8-linux-x64-shared-no-tts";
const BACKEND_SHA256: &str = "d0f96c8b65c6cd0974fada22737e337de81bc8cd2abbec2e39caf358b1eec5fc";
const MODEL_SHA256: &str = "99f63605b3a85a54c250c0869670a687b7d6598a47bf2421515e1f839a76e150";
const MODEL_NAME: &str = "parakeet-unified-en-int8";
pub const MODEL_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];
pub const MODEL_BYTES: u64 = 663_043_117;
pub const NATIVE_VERSION: &str = "4";
const TOKENIZER_REVISION: &str = "7a16ff98b72e5b0beba6f182a5357c704d977b15";
const TOKENIZER_SHA256: &str = "07d4e5a63840a53ab2d4d106d2874768143fb3fbdd47938b3910d2da05bfb0a9";
const AUDIO_MODELS: &[(&str, &str, &str)] = &[
    (
        "silero_vad.onnx",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
    ),
    (
        "gtcrn_simple.onnx",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/speech-enhancement-models/gtcrn_simple.onnx",
        "e77603ac0c23dac3227dd2d7135b3a585cbee2679048aecfa886657d3ae1b534",
    ),
];

pub fn assets_ready(values: &Config) -> bool {
    let model = Path::new(&values.model);
    let server = Path::new(&values.server);
    server.is_file()
        && fs::read_to_string(server.with_file_name("recognizer.version"))
            .is_ok_and(|version| version.trim() == NATIVE_VERSION)
        && MODEL_FILES
            .iter()
            .chain(&["bpe.vocab"])
            .all(|name| model.join(name).is_file())
        && model.parent().is_some_and(|parent| {
            AUDIO_MODELS
                .iter()
                .all(|(name, _, _)| parent.join(name).is_file())
        })
}

fn checksum(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn download(
    client: &Client,
    url: &str,
    path: &Path,
    expected: &str,
    progress: &impl Fn(&str),
) -> Result<()> {
    let name = path
        .file_name()
        .context("Missing asset filename")?
        .to_string_lossy();
    let verified = format!("Verified {name}");
    if path.is_file() && checksum(path)? == expected {
        progress(&verified);
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("Missing asset directory")?)?;
    let mut suffix = path.as_os_str().to_os_string();
    suffix.push(".part");
    let partial = PathBuf::from(suffix);
    let mut offset = match fs::symlink_metadata(&partial) {
        Ok(info) => {
            ensure!(info.is_file(), "Invalid partial download for {name}");
            info.len()
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    progress(&format!(
        "Downloading {name} (resuming at {} MiB)",
        offset / 1_048_576
    ));

    let mut response = loop {
        let mut request = client.get(url);
        if offset != 0 {
            request = request.header(header::RANGE, format!("bytes={offset}-"));
        }
        let response = request.send()?;
        if offset != 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            if checksum(&partial)? == expected {
                fs::rename(&partial, path)?;
                progress(&verified);
                return Ok(());
            }
            offset = 0;
            continue;
        }
        break response.error_for_status()?;
    };
    let append = response.status() == StatusCode::PARTIAL_CONTENT;
    if append {
        ensure!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|range| range.starts_with(&format!("bytes {offset}-"))),
            "Download returned an unexpected range for {name}"
        );
    }
    let mut downloaded = if append { offset } else { 0 };
    let mut last_update = Instant::now();
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&partial)?;
    let mut block = vec![0u8; 1024 * 1024];
    loop {
        let count = response.read(&mut block)?;
        if count == 0 {
            break;
        }
        output.write_all(&block[..count])?;
        downloaded += count as u64;
        if last_update.elapsed() >= Duration::from_secs(5) {
            progress(&format!("Downloaded {} MiB", downloaded / 1_048_576));
            last_update = Instant::now();
        }
    }
    drop(output);
    if checksum(&partial)? != expected {
        fs::remove_file(partial)?;
        bail!("SHA-256 mismatch for {name}; rejected download");
    }
    fs::rename(partial, path)?;
    progress(&verified);
    Ok(())
}

fn extract_runtime(archive: &Path, runtime: &Path) -> Result<()> {
    let mut bundle = Archive::new(BzDecoder::new(File::open(archive)?));
    for entry in bundle.entries()? {
        let mut entry = entry?;
        let source = entry.path()?.into_owned();
        let Ok(path) = source.strip_prefix(RUNTIME_NAME) else {
            continue;
        };
        if !(path.starts_with("lib") || path == Path::new("include/sherpa-onnx/c-api/c-api.h"))
            || entry.header().entry_type().is_dir()
        {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "Unsafe speech runtime archive entry"
        );
        let target = runtime.join(path);
        fs::create_dir_all(target.parent().context("Missing runtime directory")?)?;
        io::copy(&mut entry, &mut File::create_new(target)?)?;
    }
    Ok(())
}

fn stage_worker(runtime: &Path, source: &Path) -> Result<()> {
    let mut input = File::open(source).context("Rust speech worker is missing. Build and install the complete Cargo workspace, then retry.")?;
    let mut header = [0u8; 4];
    input
        .read_exact(&mut header)
        .context("Invalid Rust speech worker")?;
    ensure!(
        &header == b"\x7fELF",
        "Invalid Rust speech worker executable"
    );
    let mut staged = NamedTempFile::new_in(runtime)?;
    staged.write_all(&header)?;
    io::copy(&mut input, &mut staged)?;
    staged.as_file().sync_all()?;
    use std::os::unix::fs::PermissionsExt;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    let staged = staged.into_temp_path();
    let probe = run_command(&mut Command::new(&staged), None, Duration::from_secs(3))
        .context("Shipped Rust speech worker could not execute")?;
    ensure!(
        probe.status.code() == Some(64),
        "Shipped Rust speech worker failed its executable check"
    );
    staged.persist(runtime.join("recognizer"))?;
    fs::write(
        runtime.join("recognizer.version"),
        format!("{NATIVE_VERSION}\n"),
    )?;
    Ok(())
}

fn extract_model(archive: &Path, model: &Path) -> Result<()> {
    let mut bundle = Archive::new(BzDecoder::new(File::open(archive)?));
    fs::create_dir(model)?;
    let mut size = 0u64;
    for entry in bundle.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !MODEL_FILES.contains(&name) || !entry.header().entry_type().is_file() {
            continue;
        }
        ensure!(
            entry.size() <= MODEL_BYTES - size,
            "Unexpected model size; installation rejected"
        );
        size += io::copy(&mut entry, &mut File::create_new(model.join(name))?)?;
    }
    ensure!(
        MODEL_FILES.iter().all(|name| model.join(name).is_file()),
        "Incomplete model archive; installation rejected"
    );
    ensure!(
        size == MODEL_BYTES,
        "Unexpected model size; installation rejected"
    );
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

enum Rename {
    Move(PathBuf, PathBuf),
    Exchange(PathBuf, PathBuf),
}

impl Rename {
    fn apply(&self) -> Result<()> {
        let (source, target) = match self {
            Self::Move(source, target) => {
                fs::rename(source, target).with_context(|| {
                    format!(
                        "Could not move {} to {}",
                        source.display(),
                        target.display()
                    )
                })?;
                return Ok(());
            }
            Self::Exchange(source, target) => (source, target),
        };
        let source_c = CString::new(source.as_os_str().as_bytes())?;
        let target_c = CString::new(target.as_os_str().as_bytes())?;
        // Keep the active pathname available throughout replacement.
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source_c.as_ptr(),
                libc::AT_FDCWD,
                target_c.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("Could not replace {}", target.display()));
        }
        Ok(())
    }

    fn reverse(self) -> Self {
        match self {
            Self::Move(source, target) => Self::Move(target, source),
            exchange @ Self::Exchange(..) => exchange,
        }
    }
}

fn activate(stage: TempDir, targets: &[(PathBuf, PathBuf)]) -> Result<()> {
    let mut rollback = Vec::new();
    let result: Result<()> = (|| {
        let mut apply = |operation: Rename| -> Result<()> {
            operation.apply()?;
            rollback.push(operation.reverse());
            Ok(())
        };
        for (index, (prepared, target)) in targets.iter().enumerate() {
            if !exists(target)? {
                apply(Rename::Move(prepared.clone(), target.clone()))?;
                continue;
            }
            apply(Rename::Exchange(prepared.clone(), target.clone()))?;
            let mut previous = target.as_os_str().to_os_string();
            previous.push(".previous");
            let previous = PathBuf::from(previous);
            if exists(&previous)? {
                apply(Rename::Move(
                    previous.clone(),
                    stage.path().join(format!("older-{index}")),
                ))?;
            }
            apply(Rename::Move(prepared.clone(), previous))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for operation in rollback.into_iter().rev() {
            if let Err(restore) = operation.apply() {
                let location = stage.keep();
                bail!(
                    "{error:#}. Rollback failed: {restore:#}. Staged files saved in {}",
                    location.display()
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

pub fn install(progress: impl Fn(&str)) -> Result<(String, String)> {
    ensure!(
        cfg!(all(target_arch = "x86_64", target_os = "linux")),
        "The bundled speech runtime supports Linux x86_64"
    );
    let shipped_worker = env::current_exe()?.with_file_name("hype-flow-recognizer");
    ensure!(
        shipped_worker.is_file(),
        "Rust speech worker is missing. Build and install the complete Cargo workspace, then retry."
    );
    let root = data_dir();
    fs::create_dir_all(&root)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("setup.lock"))?;
    lock.try_lock().context("Speech setup is already running")?;

    let client = Client::builder()
        .user_agent("hype-flow-setup/0.1")
        .timeout(Duration::from_secs(60))
        .build()?;
    let downloads = root.join("downloads");
    let archive = downloads.join(format!("sherpa-onnx-{RELEASE}.tar.bz2"));
    download(
        &client,
        &format!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/v{RELEASE}/{RUNTIME_NAME}.tar.bz2"
        ),
        &archive,
        BACKEND_SHA256,
        &progress,
    )?;
    let model_archive = downloads.join(format!("{MODEL_NAME}.tar.bz2"));
    download(
        &client,
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2",
        &model_archive,
        MODEL_SHA256,
        &progress,
    )?;
    let backend = root.join("backend");
    let models = root.join("models");
    fs::create_dir_all(&backend)?;
    fs::create_dir_all(&models)?;
    for (name, url, expected) in AUDIO_MODELS {
        download(&client, url, &models.join(name), expected, &progress)?;
    }

    let stage = Builder::new().prefix(".parakeet-").tempdir_in(root)?;
    let runtime = stage.path().join("runtime");
    let model = stage.path().join("model");
    progress("Installing speech runtime");
    extract_runtime(&archive, &runtime)?;
    stage_worker(&runtime, &shipped_worker)?;
    progress("Installing Parakeet Unified English INT8");
    extract_model(&model_archive, &model)?;
    let tokenizer = downloads.join("parakeet-unified-tokenizer.model");
    download(
        &client,
        &format!(
            "https://huggingface.co/eschmidbauer/parakeet-unified-en-0.6b-onnx/resolve/{TOKENIZER_REVISION}/onnx_int8/tokenizer.model"
        ),
        &tokenizer,
        TOKENIZER_SHA256,
        &progress,
    )?;
    let vocabulary = sentencepiece_vocabulary(&fs::read(tokenizer)?, &model.join("tokens.txt"))?;
    fs::write(model.join("bpe.vocab"), vocabulary)?;

    let model_target = models.join(MODEL_NAME);
    let runtime_target = backend.join("parakeet");
    let server_target = runtime_target.join("recognizer");
    let model_path = model_target
        .to_str()
        .context("Model path is not UTF-8")?
        .to_owned();
    let server_path = server_target
        .to_str()
        .context("Speech runtime path is not UTF-8")?
        .to_owned();
    activate(stage, &[(runtime, runtime_target), (model, model_target)])?;
    progress("Speech model ready");
    Ok((model_path, server_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        thread,
    };

    #[test]
    fn stages_shipped_worker_without_a_compiler_and_rejects_bad_sources() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::write(runtime.join("recognizer"), b"previous worker").unwrap();
        assert!(stage_worker(&runtime, &root.path().join("missing")).is_err());
        let source = root.path().join("shipped-worker");
        fs::write(&source, b"not an ELF worker").unwrap();
        assert!(stage_worker(&runtime, &source).is_err());
        assert_eq!(
            fs::read(runtime.join("recognizer")).unwrap(),
            b"previous worker"
        );
        fs::write(&source, b"\x7fELFtest worker bytes").unwrap();
        assert!(stage_worker(&runtime, &source).is_err());
        assert_eq!(
            fs::read(runtime.join("recognizer")).unwrap(),
            b"previous worker"
        );
        let stub = root.path().join("worker_probe.rs");
        fs::write(&stub, "fn main() { std::process::exit(64); }").unwrap();
        assert!(
            Command::new(option_env!("RUSTC").unwrap_or("rustc"))
                .arg(&stub)
                .arg("-o")
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        stage_worker(&runtime, &source).unwrap();
        assert_eq!(
            fs::read(runtime.join("recognizer")).unwrap(),
            fs::read(&source).unwrap()
        );
        assert_eq!(
            fs::metadata(runtime.join("recognizer"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::read_to_string(runtime.join("recognizer.version")).unwrap(),
            "4\n"
        );
    }

    #[test]
    fn second_failed_activation_restores_both_active_and_older_paths() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir_in(root.path()).unwrap();
        let runtime = root.path().join("parakeet");
        let model = root.path().join("model");
        let rollback = root.path().join("parakeet.previous");
        let prepared = stage.path().join("runtime");
        let missing = stage.path().join("missing-model");
        for (path, content) in [
            (&runtime, "active"),
            (&model, "model"),
            (&rollback, "old"),
            (&prepared, "new"),
        ] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("marker"), content).unwrap();
        }
        assert!(
            activate(
                stage,
                &[(prepared, runtime.clone()), (missing, model.clone())]
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(runtime.join("marker")).unwrap(),
            "active"
        );
        assert_eq!(fs::read_to_string(model.join("marker")).unwrap(), "model");
        assert_eq!(fs::read_to_string(rollback.join("marker")).unwrap(), "old");
    }

    #[test]
    fn resumes_partial_download_and_verifies_complete_checksum() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        fs::write(root.path().join("asset.part"), b"hello ").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let source = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut range_correct = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                range_correct |= line.eq_ignore_ascii_case("range: bytes=6-\r\n");
            }
            assert!(range_correct);
            write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 6-10/11\r\nConnection: close\r\n\r\nworld").unwrap();
        });
        download(
            &Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            &url,
            &path,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            &|_| {},
        )
        .unwrap();
        source.join().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"hello world");
        assert!(!root.path().join("asset.part").exists());
    }
}
