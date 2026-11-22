use std::collections::HashSet;
use std::env;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{cleanup, vocabulary};

pub const UNLOAD_CHOICES: [u32; 6] = [0, 5, 10, 15, 30, 60];

pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

fn xdg_dir(variable: &str, home_relative: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::home_dir().unwrap_or_default().join(home_relative))
        .join("hype-flow")
}

pub fn runtime_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .filter(|base| !base.is_empty())
        .context("XDG_RUNTIME_DIR is unset. Run inside your Wayland session.")?;
    let path = PathBuf::from(base).join("hype-flow");
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("Create {}", path.display())),
    }
    let unsafe_dir = || format!("Unsafe runtime directory: {}", path.display());
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(unsafe_dir)?;
    // SAFETY: getuid has no pointer arguments or other preconditions.
    ensure!(
        directory.metadata()?.uid() == unsafe { libc::getuid() },
        "{}",
        unsafe_dir()
    );
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub server: String,
    pub language: String,
    pub prompt: String,
    pub threads: u32,
    pub gpu: bool,
    pub paste: bool,
    pub input_target: String,
    pub vocabulary_boosting: bool,
    pub boost_score: f64,
    pub auto_correct: bool,
    pub corrections: String,
    pub filter_words: bool,
    pub blocked_words: String,
    pub remove_repetitions: bool,
    pub fuzzy_vocabulary: bool,
    pub fuzzy_tolerance: f64,
    pub sounds: bool,
    pub pause_media: bool,
    pub trailing_space: bool,
    pub press_enter: bool,
    pub keep_mic_ready: bool,
    pub unload_minutes: u32,
    pub cut_silence: bool,
    pub noise_suppression: bool,
}

impl Default for Config {
    fn default() -> Self {
        let root = data_dir();
        // SAFETY: sysconf takes a constant and has no pointer arguments.
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        Self {
            model: root
                .join("models/parakeet-unified-en-int8")
                .to_string_lossy()
                .into_owned(),
            server: root
                .join("backend/parakeet/recognizer")
                .to_string_lossy()
                .into_owned(),
            language: "en".into(),
            prompt: String::new(),
            threads: if cpus > 0 { cpus.min(4) as u32 } else { 4 },
            gpu: false,
            paste: true,
            input_target: "auto".into(),
            vocabulary_boosting: false,
            boost_score: 1.5,
            auto_correct: true,
            corrections: String::new(),
            filter_words: true,
            blocked_words: cleanup::DEFAULT_FILTERS.into(),
            remove_repetitions: false,
            fuzzy_vocabulary: true,
            fuzzy_tolerance: 0.18,
            sounds: true,
            pause_media: true,
            trailing_space: false,
            press_enter: false,
            keep_mic_ready: false,
            unload_minutes: 10,
            cut_silence: true,
            noise_suppression: false,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("config.toml");
        let mut values = match read_existing(&path)? {
            Some(text) => {
                let mut supplied: toml::Table = toml::from_str(&text)?;
                let defaults = toml::Table::try_from(Self::default())?;
                let mut unknown: Vec<_> = supplied
                    .keys()
                    .filter(|key| !defaults.contains_key(*key))
                    .cloned()
                    .collect();
                unknown.sort_unstable();
                ensure!(
                    unknown.is_empty(),
                    "Unknown settings: {}",
                    unknown.join(", ")
                );

                let root = data_dir();
                for (key, previous) in [
                    ("model", "models/ggml-large-v3.bin"),
                    ("server", "backend/whisper-server-linux-x64-vulkan"),
                ] {
                    if supplied
                        .get(key)
                        .and_then(toml::Value::as_str)
                        .is_some_and(|value| OsStr::new(value) == root.join(previous).as_os_str())
                    {
                        supplied.remove(key);
                    }
                }
                supplied.try_into()?
            }
            None => Self::default(),
        };
        values.validate()?;
        values.model = resolve_path(&values.model)?;
        values.server = resolve_path(&values.server)?;
        Ok(values)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.threads),
            "threads must be between 1 and 64"
        );
        ensure!(
            (0.5..=3.0).contains(&self.boost_score),
            "Vocabulary strength must be between 0.5 and 3"
        );
        ensure!(
            (0.05..=0.4).contains(&self.fuzzy_tolerance),
            "Near-miss tolerance must be between 0.05 and 0.4"
        );
        ensure!(
            UNLOAD_CHOICES.contains(&self.unload_minutes),
            "Unload after must be one of 0, 5, 10, 15, 30, 60 minutes"
        );
        vocabulary::hotwords(&self.prompt)?;
        vocabulary::terms(&self.blocked_words)?;
        cleanup::corrections(&self.corrections)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let contents = toml::to_string(self)?;
        let directory = config_dir();
        fs::create_dir_all(&directory)?;
        let lock = File::open(&directory)?;
        lock.lock()?;
        let path = directory.join("config.toml");
        let temporary = stage_settings(&directory, &contents)?;
        if let Some(previous) = read_existing(&path)? {
            let supplied: toml::Table = toml::from_str(&previous)?;
            let changed = [("model", &self.model), ("server", &self.server)]
                .iter()
                .any(|(key, value)| {
                    supplied.get(*key).and_then(toml::Value::as_str) != Some(value.as_str())
                });
            if changed {
                stage_settings(&directory, &previous)?
                    .persist(directory.join("config.toml.previous"))?;
            }
        }
        temporary.persist(&path)?;
        lock.sync_all()?;
        Ok(())
    }
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
    }
}

fn stage_settings(directory: &Path, contents: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    Ok(file)
}

fn resolve_path(value: &str) -> Result<String> {
    ensure!(!value.contains('\0'), "Embedded NUL in speech path");
    let path = match value.strip_prefix('~') {
        Some(rest) => {
            let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
            let home = if user.is_empty() {
                env::home_dir().context("Could not determine home directory")?
            } else {
                user_home(user)?
            };
            home.join(tail.trim_start_matches('/'))
        }
        None => PathBuf::from(value),
    };
    let mut resolved = if path.is_absolute() {
        PathBuf::new()
    } else {
        env::current_dir()?
    };
    resolve_components(&mut resolved, &path, &mut HashSet::new());
    Ok(resolved.to_string_lossy().into_owned())
}

fn resolve_components(resolved: &mut PathBuf, path: &Path, active: &mut HashSet<PathBuf>) {
    for component in path.components() {
        match component {
            Component::RootDir => resolved.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                // A model path may point through a symlink before the model itself exists.
                if active.len() < 40
                    && let Ok(target) = fs::read_link(&*resolved)
                    && !active.contains(resolved)
                {
                    let link = resolved.clone();
                    active.insert(link.clone());
                    resolved.pop();
                    resolve_components(resolved, &target, active);
                    active.remove(&link);
                }
            }
            Component::Prefix(_) => unreachable!("Linux paths do not have prefixes"),
        }
    }
}

fn user_home(user: &str) -> Result<PathBuf> {
    let name = CString::new(user)?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0; 1024];
    loop {
        // SAFETY: The name is NUL-terminated and the writable storage remains live.
        let status = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        match status {
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            0 if !found.is_null() => {
                // SAFETY: getpwnam_r filled the entry and points pw_dir into the live buffer.
                let directory = unsafe { CStr::from_ptr(entry.assume_init().pw_dir) };
                return Ok(PathBuf::from(OsStr::from_bytes(directory.to_bytes())));
            }
            0 => bail!("Could not determine home directory for {user}"),
            status => return Err(io::Error::from_raw_os_error(status).into()),
        }
    }
}
