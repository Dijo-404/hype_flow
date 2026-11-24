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

use crate::{cleanup, setup, vocabulary};

pub const UNLOAD_AFTER_EACH: u32 = u32::MAX;
pub const UNLOAD_CHOICES: [u32; 9] = [0, 1, 2, 5, 10, 15, 30, 60, UNLOAD_AFTER_EACH];

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
    pub model_variant: String,
    pub server: String,
    pub language: String,
    pub prompt: String,
    pub threads: u32,
    pub feature_dim: i32,
    pub provider: String,
    pub blank_penalty: f32,
    pub max_active_paths: i32,
    pub vad_threshold: f32,
    pub vad_min_silence: f32,
    pub max_piece_seconds: u32,
    pub gpu: bool,
    pub paste: bool,
    pub modifier_release_guard: bool,
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
    pub normalize_gain: bool,
    pub recover_recordings: bool,
    pub history_enabled: bool,
    pub spoken_commands: bool,
    pub remove_restarts: bool,
    pub format_numbers: bool,
    pub capitalize_after_filler: bool,
    pub confidence_filtering: bool,
    pub confidence_threshold: f64,
    pub hallucination_filtering: bool,
    pub anomaly_threshold: f64,
    pub soft_speech_retry: bool,
    pub dictation_shortcut: String,
    pub dictation_mode: String,
    pub cancel_shortcut: String,
}

impl Default for Config {
    fn default() -> Self {
        let root = data_dir();
        let model = setup::model_spec(setup::DEFAULT_MODEL_VARIANT).expect("default speech model");
        // SAFETY: sysconf takes a constant and has no pointer arguments.
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        Self {
            model: root
                .join("models")
                .join(model.name)
                .to_string_lossy()
                .into_owned(),
            model_variant: setup::DEFAULT_MODEL_VARIANT.into(),
            server: root
                .join("backend/parakeet/recognizer")
                .to_string_lossy()
                .into_owned(),
            language: "en".into(),
            prompt: String::new(),
            threads: if cpus > 0 { cpus.min(4) as u32 } else { 4 },
            feature_dim: model.feature_dim as i32,
            provider: "cpu".into(),
            blank_penalty: 0.0,
            max_active_paths: 4,
            vad_threshold: 0.3,
            vad_min_silence: 0.45,
            max_piece_seconds: 15,
            gpu: false,
            paste: true,
            modifier_release_guard: false,
            input_target: "auto".into(),
            vocabulary_boosting: false,
            boost_score: 1.5,
            auto_correct: true,
            corrections: String::new(),
            filter_words: true,
            blocked_words: cleanup::DEFAULT_FILTERS.into(),
            remove_repetitions: false,
            fuzzy_vocabulary: false,
            fuzzy_tolerance: 0.18,
            sounds: true,
            pause_media: true,
            trailing_space: false,
            press_enter: false,
            keep_mic_ready: false,
            unload_minutes: 10,
            cut_silence: true,
            noise_suppression: false,
            normalize_gain: false,
            recover_recordings: false,
            history_enabled: false,
            spoken_commands: false,
            remove_restarts: false,
            format_numbers: false,
            capitalize_after_filler: false,
            confidence_filtering: false,
            confidence_threshold: -1.0,
            hallucination_filtering: false,
            anomaly_threshold: 3.0,
            soft_speech_retry: false,
            dictation_shortcut: "Mod+Ctrl+Space".into(),
            dictation_mode: "toggle".into(),
            cancel_shortcut: "Mod+Ctrl+Escape".into(),
        }
    }
}

impl Config {
    pub fn from_table(mut supplied: toml::Table) -> Result<Self> {
        let variant = match supplied.get("model").and_then(toml::Value::as_str) {
            Some(model) => setup::variant_for_path(Path::new(model)),
            None => supplied
                .get("model_variant")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| {
                    if supplied
                        .get("feature_dim")
                        .and_then(toml::Value::as_integer)
                        == Some(128)
                        || supplied
                            .get("vocabulary_boosting")
                            .and_then(toml::Value::as_bool)
                            == Some(true)
                    {
                        "unified"
                    } else {
                        setup::DEFAULT_MODEL_VARIANT
                    }
                }),
        };
        let model = setup::model_spec(variant)?;
        let variant = toml::Value::String(variant.into());
        supplied.entry("model_variant").or_insert(variant);
        supplied.entry("model").or_insert_with(|| {
            toml::Value::String(
                data_dir()
                    .join("models")
                    .join(model.name)
                    .to_string_lossy()
                    .into_owned(),
            )
        });
        supplied
            .entry("feature_dim")
            .or_insert(toml::Value::Integer(model.feature_dim.into()));
        Ok(supplied.try_into()?)
    }

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
                Self::from_table(supplied)?
            }
            None => Self::default(),
        };
        values.validate()?;
        values.model = resolve_path(&values.model)?;
        values.server = resolve_path(&values.server)?;
        if let Some(path) = values.provider.strip_prefix("cpu:") {
            values.provider = format!("cpu:{}", resolve_path(path)?);
        }
        Ok(values)
    }

    pub fn validate(&self) -> Result<()> {
        setup::model_spec(&self.model_variant)?;
        ensure!(
            matches!(self.dictation_mode.as_str(), "toggle" | "hold"),
            "dictation_mode must be toggle or hold"
        );
        if setup::variant_for_path(Path::new(&self.model)) != "unified" {
            ensure!(
                self.feature_dim == 80,
                "Parakeet 110M requires feature_dim = 80"
            );
            ensure!(
                !self.vocabulary_boosting,
                "Native vocabulary boosting is unavailable for Parakeet 110M. Text corrections remain available."
            );
        }
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
            "Unload after must be 0, 1, 2, 5, 10, 15, 30, 60 minutes, or after each dictation"
        );
        ensure!(
            (0.0..=2.0).contains(&self.blank_penalty),
            "blank_penalty must be between 0 and 2"
        );
        ensure!(
            (1..=16).contains(&self.max_active_paths),
            "max_active_paths must be between 1 and 16"
        );
        ensure!(
            (0.1..=0.9).contains(&self.vad_threshold),
            "vad_threshold must be between 0.1 and 0.9"
        );
        ensure!(
            (0.1..=2.0).contains(&self.vad_min_silence),
            "vad_min_silence must be between 0.1 and 2 seconds"
        );
        ensure!(
            (5..=60).contains(&self.max_piece_seconds),
            "max_piece_seconds must be between 5 and 60"
        );
        ensure!(
            matches!(self.feature_dim, 80 | 128),
            "feature_dim must be 80 or 128"
        );
        if let Some(path) = self.provider.strip_prefix("cpu:") {
            let model = fs::canonicalize(resolve_path(&self.model)?)
                .context("Model directory for provider config")?;
            let supplied =
                fs::canonicalize(resolve_path(path)?).context("Provider session config file")?;
            ensure!(
                supplied.is_file() && supplied.starts_with(model),
                "Provider session config must be inside model directory"
            );
        } else {
            ensure!(
                self.provider == "cpu",
                "Only CPU provider or a model-local CPU session config is supported"
            );
        }
        ensure!(
            self.confidence_threshold.is_finite()
                && (-20.0..=0.0).contains(&self.confidence_threshold),
            "confidence_threshold must be between -20 and 0"
        );
        ensure!(
            self.anomaly_threshold.is_finite() && (1.0..=20.0).contains(&self.anomaly_threshold),
            "anomaly_threshold must be between 1 and 20"
        );
        vocabulary::hotwords(&self.prompt)?;
        vocabulary::terms(&self.blocked_words)?;
        cleanup::corrections(&self.corrections)?;
        let dictate = crate::shortcuts::normalize(&self.dictation_shortcut)?;
        let cancel = crate::shortcuts::normalize(&self.cancel_shortcut)?;
        ensure!(
            !dictate.eq_ignore_ascii_case(&cancel),
            "Dictate and Cancel must use different shortcuts"
        );
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
