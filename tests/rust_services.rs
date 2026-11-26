use hype_flow::{
    config::Config,
    feedback::{MediaPause, play_cue},
    hook, output, setup, shortcuts,
};
use serde_json::json;
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn executable(path: &Path, script: &str) {
    fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait_for(path: &Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(path).as_deref().ok() != Some(expected) {
        assert!(
            Instant::now() < deadline,
            "expected {expected:?} in {path:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn terminal_matching_checks_short_names_as_words() {
    for app in ["Alacritty", "org.gnome.Console", "St", "org.example.st"] {
        assert!(output::is_terminal(app), "{app}");
    }
    for app in ["steam", "firefox", "", "org.example.station"] {
        assert!(!output::is_terminal(app), "{app}");
    }
}

#[test]
fn code_editors_use_the_openwhispr_shift_insert_route() {
    for app in [
        "code",
        "Code - Insiders",
        "code-oss",
        "com.microsoft.VSCode",
        "cursor",
        "VSCodium",
    ] {
        assert!(output::use_shift_insert(app), "{app}");
    }
    for app in [
        "firefox",
        "Alacritty",
        "org.example.decode",
        "my-code-editor",
        "",
    ] {
        assert!(!output::use_shift_insert(app), "{app}");
    }
}

#[test]
fn shortcuts_normalize_and_reject_unsafe_or_duplicate_bindings() {
    assert_eq!(shortcuts::normalize(" alt + space ").unwrap(), "Alt+Space");
    assert_eq!(
        shortcuts::normalize("Shift+Control+Escape").unwrap(),
        "Ctrl+Shift+Escape"
    );
    assert_eq!(shortcuts::normalize("Super+Space").unwrap(), "Super+Space");
    for invalid in [
        "",
        "Space",
        "Alt+",
        "Alt+Ctrl",
        "Ctrl+Control+Space",
        "Alt+Space\nspawn",
        "Alt+Space { quit; }",
    ] {
        assert!(shortcuts::normalize(invalid).is_err(), "{invalid:?}");
    }
    let values = Config {
        dictation_shortcut: "Alt+Space".into(),
        cancel_shortcut: "alt + space".into(),
        ..Config::default()
    };
    assert!(
        values
            .validate()
            .unwrap_err()
            .to_string()
            .contains("different")
    );
}

#[test]
fn services_use_isolated_environment() {
    if let Some(root) = env::var_os("HYPE_FLOW_TEST_SERVICES") {
        exercise_services(Path::new(&root));
        return;
    }
    let root = tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "services_use_isolated_environment",
            "--nocapture",
        ])
        .env("HYPE_FLOW_TEST_SERVICES", root.path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("NIRI_CONFIG", root.path().join("config/niri/config.kdl"))
        .env(
            "PATH",
            env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exercise_services(root: &Path) {
    let bin = root.join("bin");
    executable(
        &bin.join("niri"),
        r#"case "$1:$2:$3" in
        msg:--json:focused-window)
            printf 'niri\n' >> "$HOME/spawned"
            cat "$HOME/focus" ;;
        msg:action:focus-window)
            [ ! -f "$HOME/closed" ] || exit 1
            printf '{"id":%s,"app_id":"firefox"}' "$5" > "$HOME/focus" ;;
        validate:--config:*)
            grep -q broken "$3" && { echo 'bad config' >&2; exit 1; }
            staged=$(sed -n 's/^include "\(.*\/bindings.kdl\)"$/\1/p' "$3")
            if [ -f "$HOME/reject-hold" ] && grep -q 'press {' "$staged"; then
                echo 'press/release unavailable' >&2; exit 1
            fi
            grep -Eq '^include ".*/bindings.kdl"$' "$3" ;;
        *) exit 2 ;;
        esac"#,
    );
    executable(
        &bin.join("wl-paste"),
        r#"printf 'wl-paste\n' >> "$HOME/spawned"
        selection_file="$HOME/clipboard"
        if [ "$1" = --primary ]; then selection_file="$HOME/primary"; shift; fi
        if [ ! -f "$selection_file" ]; then echo 'Nothing is copied' >&2; exit 1; fi
        case "$*" in
        --list-types) printf 'image/png\ntext/plain;charset=utf-8\n' ;;
        '--no-newline --type text'|'--no-newline --type text/plain;charset=utf-8') cat "$selection_file" ;;
        *) exit 2 ;;
        esac"#,
    );
    executable(
        &bin.join("wl-copy"),
        r#"[ "$CLIPBOARD_STATE" = sensitive ] || exit 1
        sensitive=0
        if [ "$1" = --sensitive ]; then sensitive=1; shift; fi
        selection_file="$HOME/clipboard"
        if [ "$1" = --primary ]; then selection_file="$HOME/primary"; shift; fi
        if [ "$1" != --clear ]; then [ "$sensitive" = 1 ] || exit 4; fi
        case "$*" in
        --clear) rm -f "$selection_file" ;;
        '--type text/plain;charset=utf-8')
            cat > "$selection_file"
            if [ -f "$HOME/new-selection" ]; then mv "$HOME/new-selection" "$selection_file"; fi
            if [ -f "$HOME/held-stderr" ]; then
                (while [ -f "$HOME/held-stderr" ]; do sleep 0.02; done) </dev/null >/dev/null &
            fi ;;
        '--type text/plain;charset=utf-8 --primary') cat > "$HOME/primary" ;;
        *) exit 2 ;;
        esac"#,
    );
    executable(
        &bin.join("wtype"),
        r#"printf '%s\n' "$*" >> "$HOME/typing""#,
    );

    let focus = root.join("focus");
    let clipboard = root.join("clipboard");
    let typing = root.join("typing");
    let write_focus = |id| fs::write(&focus, format!(r#"{{"id":{id}}}"#)).unwrap();
    let firefox = json!({"id": 1, "app_id": "firefox"});
    write_focus(2);
    fs::write(&clipboard, "previous").unwrap();
    assert_eq!(
        output::deliver("hello", Some(&firefox), true, true, true).unwrap(),
        "Pasted and sent",
    );
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "hello ");
    assert_eq!(
        fs::read_to_string(&typing).unwrap(),
        "-M ctrl -P v -p v -m ctrl\n-k Return\n"
    );
    wait_for(&clipboard, "previous");

    output::deliver("first rapid paste", Some(&firefox), true, false, false).unwrap();
    output::deliver("second rapid paste", Some(&firefox), true, false, false).unwrap();
    output::finish_pending();
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "previous");
    fs::remove_file(root.join("spawned")).unwrap();
    output::deliver("checked once", Some(&firefox), true, false, false).unwrap();
    // Save the clipboard, confirm it still holds the transcript, then check focus once.
    assert_eq!(
        fs::read_to_string(root.join("spawned")).unwrap(),
        "wl-paste\nwl-paste\nwl-paste\nniri\n"
    );
    output::finish_pending();
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "previous");

    let code = json!({"id": 1, "app_id": "com.microsoft.VSCode"});
    let primary = root.join("primary");
    fs::write(&primary, "previous primary").unwrap();
    output::deliver("Codex webview text", Some(&code), true, false, false).unwrap();
    assert_eq!(fs::read_to_string(&primary).unwrap(), "Codex webview text");
    assert!(
        fs::read_to_string(&typing)
            .unwrap()
            .ends_with("-M shift -k Insert -m shift\n")
    );
    output::deliver("rapid Codex text", Some(&code), true, false, false).unwrap();
    output::finish_pending();
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "previous");
    assert_eq!(fs::read_to_string(&primary).unwrap(), "previous primary");
    output::deliver("keep newer selection", Some(&code), true, false, false).unwrap();
    fs::write(&primary, "copied separately in primary").unwrap();
    output::finish_pending();
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "previous");
    assert_eq!(
        fs::read_to_string(&primary).unwrap(),
        "copied separately in primary"
    );
    output::deliver("manual primary copy", Some(&code), true, false, false).unwrap();
    output::copy_text("explicit primary copy", true).unwrap();
    output::finish_pending();
    assert_eq!(
        fs::read_to_string(&primary).unwrap(),
        "explicit primary copy"
    );
    fs::write(&primary, "primary before failed keyboard").unwrap();
    executable(&bin.join("wtype"), "exit 1");
    assert!(output::deliver("failed Code paste", Some(&code), true, false, false).is_err());
    output::finish_pending();
    assert_eq!(
        fs::read_to_string(&primary).unwrap(),
        "primary before failed keyboard"
    );
    assert!(
        output::deliver_until(
            "cancel Code before keys",
            Some(&code),
            true,
            false,
            false,
            || fs::read_to_string(&primary).is_ok_and(|value| value == "cancel Code before keys")
        )
        .is_err()
    );
    output::finish_pending();
    assert_eq!(
        fs::read_to_string(&primary).unwrap(),
        "primary before failed keyboard"
    );
    executable(
        &bin.join("wtype"),
        r#"printf '%s\n' "$*" >> "$HOME/typing""#,
    );
    fs::remove_file(&typing).unwrap();
    let began = Instant::now();
    assert!(
        output::deliver_until(
            "must not paste",
            Some(&firefox),
            true,
            false,
            true,
            || began.elapsed() >= Duration::from_millis(60)
        )
        .is_err()
    );
    assert!(
        !typing.exists(),
        "Shutdown during delivery must not type keys"
    );

    executable(
        &bin.join("wtype"),
        r#"printf %s "$$" > "$HOME/typing-pid"; sleep 0.5; printf typed > "$HOME/typing""#,
    );
    assert!(
        output::deliver_until(
            "cancel stalled keyboard",
            Some(&firefox),
            true,
            false,
            false,
            || root.join("typing-pid").is_file()
        )
        .is_err()
    );
    assert!(
        !typing.exists(),
        "Shutdown must kill a stalled keyboard child"
    );
    let keyboard_pid = fs::read_to_string(root.join("typing-pid")).unwrap();
    assert!(!Path::new("/proc").join(keyboard_pid).exists());
    executable(
        &bin.join("wtype"),
        r#"printf '%s\n' "$*" >> "$HOME/typing""#,
    );

    let terminal = json!({"id": 1, "app_id": "Alacritty"});
    assert_eq!(
        output::deliver("terminal", Some(&terminal), true, false, true).unwrap(),
        "Pasted"
    );
    assert!(
        fs::read_to_string(&typing)
            .unwrap()
            .ends_with("-M ctrl -M shift -P v -p v -m shift -m ctrl\n")
    );
    fs::write(&clipboard, "copied separately").unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "copied separately");

    fs::remove_file(&typing).unwrap();
    fs::write(root.join("closed"), []).unwrap();
    write_focus(2);
    assert_eq!(
        output::deliver("recover me", Some(&firefox), true, false, true).unwrap(),
        "Copied · focus changed"
    );
    assert!(!typing.exists());
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "recover me");
    assert_eq!(
        fs::read_to_string(root.join("primary")).unwrap(),
        "recover me"
    );
    write_focus(1);
    fs::write(root.join("new-selection"), "new clipboard item").unwrap();
    assert_eq!(
        output::deliver("recover this", Some(&firefox), true, false, false).unwrap(),
        "Copied · clipboard changed"
    );
    assert!(!typing.exists());
    assert_eq!(
        fs::read_to_string(&clipboard).unwrap(),
        "new clipboard item"
    );
    assert_eq!(
        fs::read_to_string(root.join("primary")).unwrap(),
        "recover this"
    );
    let held_stderr = root.join("held-stderr");
    fs::write(&held_stderr, []).unwrap();
    let copied = output::copy_text("daemon selection", false);
    fs::remove_file(held_stderr).unwrap();
    copied.unwrap();
    assert_eq!(fs::read_to_string(&clipboard).unwrap(), "daemon selection");

    let path = hook::hook_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert_eq!(hook::run_hook("cleaned"), ("cleaned".into(), None));
    executable(&path, "tr a-z A-Z");
    assert_eq!(hook::run_hook("cleaned"), ("CLEANED".into(), None));
    executable(
        &path,
        "echo 'transcript stderr should stay private' >&2; exit 3",
    );
    let (text, note) = hook::run_hook("cleaned");
    assert_eq!(text, "cleaned");
    assert!(note.unwrap().contains("status 3"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o722)).unwrap();
    let (text, note) = hook::run_hook("cleaned");
    assert_eq!(text, "cleaned");
    assert!(note.unwrap().contains("skipped"));
    let hook_pid = root.join("hook-pid");
    executable(&path, r#"printf %s "$$" > "$HOME/hook-pid"; exec sleep 60"#);
    let start = Instant::now();
    let (text, note) = hook::run_hook_until("cleaned", || {
        fs::read_to_string(&hook_pid).is_ok_and(|pid| !pid.is_empty())
    });
    assert_eq!(text, "cleaned");
    assert_eq!(note.as_deref(), Some("transcript hook cancelled"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(
        !Path::new("/proc")
            .join(fs::read_to_string(hook_pid).unwrap())
            .exists()
    );

    let launcher = root.join(".local/bin/hype-flow");
    fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    executable(&launcher, "exit 0");
    let niri = root.join("config/niri");
    fs::create_dir_all(&niri).unwrap();
    let config = niri.join("config.kdl");
    let bindings = niri.join("hype-flow.kdl");
    fs::write(&config, "broken {\n").unwrap();
    assert!(
        shortcuts::install()
            .unwrap_err()
            .to_string()
            .contains("niri rejected")
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), "broken {\n");
    assert!(!bindings.exists());
    fs::write(&config, "include \"keys.kdl\"\n").unwrap();
    fs::write(
        niri.join("keys.kdl"),
        "binds { Alt+Space { spawn \"test\"; }; }\n",
    )
    .unwrap();
    assert!(
        shortcuts::install()
            .unwrap_err()
            .to_string()
            .contains("keys.kdl")
    );
    assert!(!bindings.exists());
    fs::write(
        niri.join("keys.kdl"),
        "binds { Mod+Shift+D { spawn \"test\"; }; }\n",
    )
    .unwrap();
    let preferences = root.join("config/hype-flow/config.toml");
    fs::create_dir_all(&preferences).unwrap();
    let original_config = fs::read_to_string(&config).unwrap();
    assert!(shortcuts::save(&Config::default()).is_err());
    assert_eq!(fs::read_to_string(&config).unwrap(), original_config);
    assert!(!bindings.exists());
    fs::remove_dir(&preferences).unwrap();
    shortcuts::install().unwrap();
    shortcuts::install().unwrap();
    assert_eq!(
        fs::read_to_string(&config)
            .unwrap()
            .matches("hype-flow.kdl")
            .count(),
        1
    );
    assert!(fs::read_dir(&niri).unwrap().flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("config.kdl.hype-flow-backup-")
    }));

    let mut values = Config {
        dictation_shortcut: "Mod+Ctrl+Space".into(),
        ..Config::default()
    };
    let original_bindings = fs::read_to_string(&bindings).unwrap();
    assert!(original_bindings.contains("Alt+Space"));
    fs::create_dir_all(&preferences).unwrap();
    assert!(shortcuts::save(&values).is_err());
    assert_eq!(fs::read_to_string(&bindings).unwrap(), original_bindings);
    fs::remove_dir(&preferences).unwrap();
    shortcuts::save(&values).unwrap();
    assert_eq!(Config::load().unwrap().dictation_shortcut, "Mod+Ctrl+Space");
    let installed = fs::read_to_string(&bindings).unwrap();
    shortcuts::install().unwrap();
    assert_eq!(fs::read_to_string(&bindings).unwrap(), installed);
    assert!(installed.contains("Mod+Ctrl+Space"));
    assert!(!installed.contains("Alt+Space"));
    assert!(installed.contains("Mod+Ctrl+Escape"));
    let saved_toggle_preferences = fs::read(&preferences).unwrap();
    fs::write(root.join("reject-hold"), "unsupported compositor").unwrap();
    values.dictation_mode = "hold".into();
    assert!(
        shortcuts::save(&values)
            .unwrap_err()
            .to_string()
            .contains("Push-to-talk requires niri")
    );
    assert_eq!(fs::read_to_string(&bindings).unwrap(), installed);
    assert_eq!(fs::read(&preferences).unwrap(), saved_toggle_preferences);
    fs::remove_file(root.join("reject-hold")).unwrap();
    shortcuts::save(&values).unwrap();
    let held = fs::read_to_string(&bindings).unwrap();
    assert!(held.contains("dev.hype.Flow press") && held.contains("dev.hype.Flow release"));
    assert_eq!(Config::load().unwrap().dictation_mode, "hold");
    values.dictation_mode = "toggle".into();
    shortcuts::save(&values).unwrap();
    assert_eq!(fs::read_to_string(&bindings).unwrap(), installed);
    assert!(fs::read_dir(&niri).unwrap().flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("hype-flow.kdl.hype-flow-backup-")
            && fs::read_to_string(entry.path()).is_ok_and(|text| text == original_bindings)
    }));
    fs::write(
        niri.join("keys.kdl"),
        "binds { Alt+F9 { spawn \"test\"; }; }\n",
    )
    .unwrap();
    let saved_preferences = fs::read(&preferences).unwrap();
    values.dictation_shortcut = "Alt+F9".into();
    assert!(
        shortcuts::save(&values)
            .unwrap_err()
            .to_string()
            .contains("keys.kdl")
    );
    assert_eq!(fs::read(&preferences).unwrap(), saved_preferences);
    assert_eq!(fs::read_to_string(&bindings).unwrap(), installed);

    executable(
        &bin.join("playerctl"),
        r#"case "$*" in
        "--all-players --format {{playerInstance}}$(printf '\t'){{status}} status")
            printf 'playing\tPlaying\nresting\tPaused\nfailed\tPlaying\n' ;;
        '--player failed pause') exit 1 ;;
        '--player playing play') sleep 0.1; printf '%s\n' "$*" >> "$HOME/player-actions" ;;
        *) printf '%s\n' "$*" >> "$HOME/player-actions" ;;
        esac"#,
    );
    let mut media = MediaPause::default();
    media.pause();
    media.resume();
    drop(media);
    assert_eq!(
        fs::read_to_string(root.join("player-actions")).unwrap(),
        "--player playing pause\n--player playing play\n"
    );

    executable(
        &bin.join("pw-play"),
        r#"[ "$1:$2" = "--media-role:Notification" ] || exit 2
        printf %s "$3" > "$HOME/played-cue""#,
    );
    play_cue("start", root).unwrap();
    let cue = root.join("cue-start.wav");
    wait_for(&root.join("played-cue"), cue.to_str().unwrap());
    let wav = hound::WavReader::open(cue).unwrap();
    assert_eq!(wav.spec().channels, 1);
    assert_eq!(wav.spec().sample_rate, 48_000);
    assert_eq!(wav.spec().bits_per_sample, 16);
    assert_eq!(wav.duration(), 11_040);
}

#[test]
fn cached_worker_version_and_audio_assets_are_required() {
    let root = tempdir().unwrap();
    let model = root.path().join("models/parakeet");
    let worker = root.path().join("recognizer");
    fs::create_dir_all(&model).unwrap();
    let values = Config {
        model: model.to_string_lossy().into_owned(),
        server: worker.to_string_lossy().into_owned(),
        ..Config::default()
    };
    for name in setup::MODEL_FILES.iter().copied().chain(["bpe.vocab"]) {
        fs::write(model.join(name), []).unwrap();
    }
    executable(&worker, "exit 0");
    let version = worker.with_file_name("recognizer.version");
    fs::write(&version, format!("{}\n", setup::NATIVE_VERSION)).unwrap();
    assert!(!setup::assets_ready(&values));
    fs::write(model.parent().unwrap().join("silero_vad.onnx"), []).unwrap();
    assert!(!setup::assets_ready(&values));
    fs::write(model.parent().unwrap().join("gtcrn_simple.onnx"), []).unwrap();
    fs::write(&version, "3\n").unwrap();
    assert!(!setup::assets_ready(&values));
    fs::write(&version, format!("{}\n", setup::NATIVE_VERSION)).unwrap();
    assert!(setup::assets_ready(&values));
}
