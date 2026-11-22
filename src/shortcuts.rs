use crate::{
    config::{Config, config_dir},
    process::run_command,
};
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::{Builder, NamedTempFile};

const HEADER: &str = "// Managed by Hype Flow.\n";
static KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)(?:^|[;{}])\s*"?([A-Za-z0-9_]+(?:\+[A-Za-z0-9_]+)+)"?(?:\s|\{)"#).unwrap()
});
static INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*include\s+("(?:[^"\\]|\\.)*")"#).unwrap());

pub fn normalize(shortcut: &str) -> Result<String> {
    ensure!(shortcut.len() <= 120, "Shortcut is too long");
    let parts: Vec<_> = shortcut.split('+').map(str::trim).collect();
    ensure!(
        parts.len() >= 2,
        "Use a modifier and a key, for example Alt+Space"
    );
    let modifier = |part: &str| match part.to_ascii_lowercase().as_str() {
        "mod" => Some("Mod"),
        "super" | "win" => Some("Super"),
        "ctrl" | "control" => Some("Ctrl"),
        "alt" => Some("Alt"),
        "shift" => Some("Shift"),
        "mod5" | "iso_level3_shift" => Some("Mod5"),
        "iso_level5_shift" => Some("ISO_Level5_Shift"),
        _ => None,
    };
    let mut modifiers = HashSet::new();
    for part in &parts[..parts.len() - 1] {
        let name = modifier(part)
            .context("Unknown shortcut modifier. Use Alt, Ctrl, Shift, Super or Mod")?;
        ensure!(
            modifiers.insert(name),
            "Shortcut repeats the {name} modifier"
        );
    }
    let key = parts.last().copied().unwrap_or_default();
    ensure!(
        !key.is_empty()
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && modifier(key).is_none(),
        "Shortcut needs a key name such as Space, Escape or F9"
    );
    let key = match key.to_ascii_lowercase().as_str() {
        "space" => "Space",
        "esc" | "escape" => "Escape",
        "enter" | "return" => "Return",
        "tab" => "Tab",
        _ => key,
    };
    let mut result: Vec<_> = [
        "Mod",
        "Super",
        "Ctrl",
        "Alt",
        "Shift",
        "Mod5",
        "ISO_Level5_Shift",
    ]
    .into_iter()
    .filter(|name| modifiers.contains(name))
    .collect();
    result.push(key);
    Ok(result.join("+"))
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .context("niri configuration path is not UTF-8")
}

fn optional_contents(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn check_conflicts(
    path: &Path,
    managed: &Path,
    keys: &[String],
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    if path == managed {
        return Ok(());
    }
    let resolved = path
        .canonicalize()
        .with_context(|| format!("Could not read niri config {}", path.display()))?;
    if resolved == managed || !visited.insert(resolved) {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    for found in KEY.captures_iter(&content) {
        if normalize(&found[1]).is_ok_and(|key| {
            keys.iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&key))
        }) {
            bail!(
                "{} is already bound in {}. Existing shortcuts were kept.",
                &found[1],
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for included in INCLUDE.captures_iter(&content) {
        let included: String = serde_json::from_str(&included[1])?;
        check_conflicts(&parent.join(included), managed, keys, visited)?;
    }
    Ok(())
}

fn bindings(launcher: &Path, keys: &[String; 2]) -> Result<String> {
    let launcher = path_string(launcher)?;
    let executable = serde_json::to_string(launcher)?;
    let quoted = format!("'{}'", launcher.replace('\'', "'\"'\"'"));
    let command = |action| -> Result<String> {
        // The action reaches the running app without waiting for a new process to load.
        let script = format!(
            "gapplication action dev.hype.Flow {action} 2>/dev/null || exec {quoted} --{action}"
        );
        Ok(format!(
            "spawn \"sh\" \"-c\" {}",
            serde_json::to_string(&script)?
        ))
    };
    Ok(format!(
        r#"{HEADER}spawn-at-startup {executable} "--background"
binds {{
    {} repeat=false hotkey-overlay-title="Dictate offline" {{ {}; }}
    {} repeat=false hotkey-overlay-title="Cancel dictation" {{ {}; }}
}}
window-rule {{
    match app-id="^dev[.]hype[.]Flow$"
    open-floating true
    default-column-width {{ fixed 960; }}
    default-window-height {{ fixed 680; }}
}}
"#,
        keys[0],
        command("toggle")?,
        keys[1],
        command("cancel")?
    ))
}

pub fn install() -> Result<String> {
    install_with(None)
}

pub fn save(values: &Config) -> Result<String> {
    install_with(Some(values))
}

fn install_with(changes: Option<&Config>) -> Result<String> {
    let launcher = env::home_dir()
        .context("Could not find your home directory")?
        .join(".local/bin/hype-flow");
    ensure!(
        launcher.is_file(),
        "Install Hype Flow before enabling desktop shortcuts"
    );
    let config = env::var_os("NIRI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir().with_file_name("niri").join("config.kdl"));
    let config = if config.is_absolute() {
        config
    } else {
        env::current_dir()?.join(config)
    };
    let parent = config
        .parent()
        .context("Missing niri configuration directory")?;
    let lock = fs::File::open(parent)?;
    lock.lock()?;
    // Read saved shortcuts under the same lock so an older Configure request cannot overwrite a newer save.
    let saved;
    let values = if let Some(values) = changes {
        values
    } else {
        saved = Config::load()?;
        &saved
    };
    values.validate()?;
    let keys = [
        normalize(&values.dictation_shortcut)?,
        normalize(&values.cancel_shortcut)?,
    ];
    let target = parent.join("hype-flow.kdl");
    let current_bindings = optional_contents(&target)?;
    ensure!(
        current_bindings
            .as_ref()
            .is_none_or(|text| text.starts_with(HEADER)),
        "hype-flow.kdl already exists and belongs to another configuration"
    );
    let original = fs::read_to_string(&config)?;
    let include = format!("include {}", serde_json::to_string(path_string(&target)?)?);
    let managed = if target.exists() {
        target.canonicalize()?
    } else {
        parent.canonicalize()?.join("hype-flow.kdl")
    };
    check_conflicts(&config, &managed, &keys, &mut HashSet::new())?;

    let temporary = Builder::new().prefix(".hype-flow-").tempdir_in(parent)?;
    let generated = temporary.path().join("bindings.kdl");
    fs::write(&generated, bindings(&launcher, &keys)?)?;
    let staged_include = format!(
        "include {}",
        serde_json::to_string(path_string(&generated)?)?
    );
    let included = original.contains(&include);
    let checked = if included {
        original.replace(&include, &staged_include)
    } else {
        format!("{original}\n{staged_include}\n")
    };
    // Validate beside the real config so that relative includes resolve identically.
    let candidate = Builder::new()
        .prefix(".hype-flow-check-")
        .suffix(".kdl")
        .tempfile_in(parent)?;
    fs::write(candidate.path(), checked)?;
    let result = run_command(
        Command::new("niri")
            .args(["validate", "--config"])
            .arg(candidate.path()),
        None,
        Duration::from_secs(10),
    )?;
    if !result.status.success() {
        let diagnostic = String::from_utf8_lossy(if result.stderr.is_empty() {
            &result.stdout
        } else {
            &result.stderr
        });
        let first = diagnostic
            .char_indices()
            .rev()
            .nth(699)
            .map_or(0, |(offset, _)| offset);
        bail!(
            "niri rejected the shortcut configuration: {}",
            &diagnostic[first..]
        );
    }
    ensure!(
        fs::read_to_string(&config)? == original && optional_contents(&target)? == current_bindings,
        "niri configuration changed during validation. Existing shortcuts were kept."
    );
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let filename = config.file_name().unwrap_or_default().to_string_lossy();
    let backup = parent.join(format!("{filename}.hype-flow-backup-{stamp}"));
    let backup_copy = NamedTempFile::new_in(parent)?;
    fs::copy(&config, backup_copy.path())?;
    backup_copy.persist_noclobber(&backup)?;
    if current_bindings.is_some() {
        let backup_copy = NamedTempFile::new_in(parent)?;
        fs::copy(&target, backup_copy.path())?;
        backup_copy
            .persist_noclobber(parent.join(format!("hype-flow.kdl.hype-flow-backup-{stamp}")))?;
    }

    let previous = temporary.path().join("previous-bindings.kdl");
    if current_bindings.is_some() {
        fs::hard_link(&target, &previous)?;
    }
    if included {
        fs::rename(generated, &target)?;
    } else {
        let updated = temporary.path().join("config.kdl");
        fs::write(&updated, format!("{original}\n{include}\n"))?;
        fs::set_permissions(&updated, fs::metadata(&config)?.permissions())?;
        fs::rename(generated, &target)?;
        if let Err(error) = fs::rename(updated, &config) {
            let rollback = if current_bindings.is_some() {
                fs::rename(previous, target)
            } else {
                fs::remove_file(target)
            };
            if let Err(rollback) = rollback {
                let location = temporary.keep();
                bail!(
                    "Could not activate shortcuts: {error}. Rollback failed: {rollback}. Files saved in {}",
                    location.display()
                );
            }
            return Err(error).context("Could not activate shortcuts");
        }
    }
    if let Err(error) = if changes.is_some() {
        values.save()
    } else {
        Ok(())
    } {
        let rollback = (|| -> Result<()> {
            if current_bindings.is_some() {
                fs::rename(&previous, &target)?;
            } else {
                fs::remove_file(&target)?;
            }
            if !included {
                let restore = temporary.path().join("restore-config.kdl");
                fs::copy(&backup, &restore)?;
                fs::rename(restore, &config)?;
            }
            Ok(())
        })();
        if let Err(rollback) = rollback {
            let location = temporary.keep();
            bail!(
                "Could not save settings: {error}. Shortcut rollback failed: {rollback}. Files saved in {}",
                location.display()
            );
        }
        return Err(error).context("Could not save settings; previous shortcuts restored");
    }
    Ok(format!(
        "Shortcuts ready. {} starts dictation. A backup of your niri config was saved.",
        keys[0]
    ))
}
