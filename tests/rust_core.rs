use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

use hype_flow::{
    cleanup,
    config::{self, Config},
    segments, vocabulary,
};

#[test]
fn cleanup_matches_regression_fixtures() {
    #[derive(serde::Deserialize)]
    struct Case {
        input: String,
        settings: Config,
        output: String,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/cleanup.json")).unwrap();
    for case in cases {
        assert_eq!(
            cleanup::process(&case.input, &case.settings).unwrap(),
            case.output,
            "{:?}",
            case.input,
        );
    }
}

#[test]
fn correction_validation_enforces_format_limits_and_casefolded_uniqueness() {
    for rules in [
        "invalid",
        "word => ",
        " => word",
        "one => a\nONE => b",
        ". => end",
        "Straße => a\r\nSTRASSE => b",
        "word\t=> target",
        "word => ta\0rget",
    ] {
        let values = Config {
            corrections: rules.into(),
            ..Config::default()
        };
        assert!(values.validate().is_err(), "{rules:?}");
    }
    for rules in [
        format!("{} => replacement", "å".repeat(161)),
        format!("word => {}", " ".repeat(33000)),
        (0..101)
            .map(|i| format!("word{i} => replacement\n"))
            .collect(),
    ] {
        let values = Config {
            corrections: rules,
            ..Config::default()
        };
        assert!(values.validate().is_err());
    }
}

#[test]
fn vocabulary_deduplicates_with_unicode_casefolding_and_checks_limits() {
    assert_eq!(
        vocabulary::terms(" niri, OpenAI\nNIRI, Straße, STRASSE\rœ\u{1f}word ").unwrap(),
        ["niri", "OpenAI", "Straße", "œ word"],
    );
    assert_eq!(
        vocabulary::hotwords("niri, OpenAI").unwrap(),
        "niri\nNiri\nOpenAI\nopenai"
    );
    assert_eq!(vocabulary::hotwords("ß").unwrap(), "ß\nSS");
    assert_eq!(vocabulary::terms(&"å".repeat(80)).unwrap().len(), 1);
    assert!(vocabulary::terms(&"å".repeat(81)).is_err());
    assert!(vocabulary::terms(&" ".repeat(20001)).is_err());
    assert!(vocabulary::terms("bad\0word").is_err());
    let words = (0..101).map(|i| format!("word{i},")).collect::<String>();
    assert!(vocabulary::terms(&words).is_err());
    assert!(vocabulary::hotwords("smile :999").is_err());
}

#[test]
fn sentencepiece_retains_score_precision_and_protobuf_order() {
    let root = tempfile::tempdir().unwrap();
    let tokens = root.path().join("tokens.txt");
    fs::write(&tokens, "b 0\na 1\nc 2\nd 3\n<blk> 4\n").unwrap();
    let mut model = vec![];
    for (piece, score) in [("a", -1.0_f32), ("b", 0.1), ("c", 1.0e-6), ("d", -0.0)] {
        let mut value = vec![0x0a, 1, piece.as_bytes()[0], 0x15];
        value.extend(score.to_le_bytes());
        model.extend([0x0a, value.len() as u8]);
        model.extend(value);
    }
    assert_eq!(
        vocabulary::sentencepiece_vocabulary(&model, &tokens).unwrap(),
        "a\t-1\nb\t0.100000001\nc\t9.99999997e-07\nd\t-0\n",
    );
    fs::write(&tokens, "a 0\n<blk> 1\n").unwrap();
    assert!(
        vocabulary::sentencepiece_vocabulary(&model, &tokens)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn malformed_tokenizer_fields_and_pieces_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let tokens = root.path().join("tokens.txt");
    fs::write(&tokens, "a 0\n").unwrap();
    for data in [
        &b""[..],
        b"\x0a\xff",
        b"\x0a\x05x",
        b"\x00",
        b"\x0b",
        b"\x0a\x02\x0a\x00",
        b"\x0a\x08\x0a\x01\xff\x15\0\0\0\0",
        b"\x0a\x08\x0a\x01 \x15\0\0\0\0",
        b"\x0a\x08\x0a\x01a\x15\0\0\x80\x7f",
        b"\x0a\x08\x0a\x01a\x15\0\0\0\0\x0a\x08\x0a\x01a\x15\0\0\0\0",
    ] {
        assert!(
            vocabulary::sentencepiece_vocabulary(data, &tokens).is_err(),
            "{data:?}"
        );
    }
    assert!(vocabulary::sentencepiece_vocabulary(&vec![0; 1_000_001], &tokens).is_err());
}

#[test]
fn segmentation_preserves_samples_and_uses_the_quiet_run_midpoint() {
    let loud = 1000_i16.to_le_bytes().repeat(16000);
    let pcm = [loud.repeat(12), vec![0; 16000], loud.repeat(8)].concat();
    let slices = segments::split_at_pauses_with_limit(&pcm, 15);
    assert_eq!(
        slices.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
        [392_000, 264_000]
    );
    assert_eq!(slices.concat(), pcm);
    assert_eq!(slices[0].as_ptr(), pcm.as_ptr());
    let pcm = (-100i16..100)
        .flat_map(i16::to_le_bytes)
        .collect::<Vec<_>>()
        .repeat(5000);
    let slices = segments::split_at_pauses_with_limit(&pcm, 15);
    assert_eq!(slices.concat(), pcm);
    assert!(slices.iter().all(|slice| slice.len() <= 480_000));
    let pcm = 1i16.to_le_bytes().repeat(240_000);
    assert_eq!(segments::split_at_pauses_with_limit(&pcm, 15), [&pcm[..]]);
    assert!(segments::split_at_pauses_with_limit(&[], 15).is_empty());
}

#[test]
fn config_paths_validation_and_backup_contract() {
    if let Some(root) = std::env::var_os("HYPE_FLOW_CORE_TEST_ROOT") {
        check_config_paths(&std::path::PathBuf::from(root));
        return;
    }
    // Keep XDG overrides local to a child process while other tests run.
    let root = tempfile::tempdir().unwrap();
    let mut check = Command::new(std::env::current_exe().unwrap());
    check
        .args([
            "--exact",
            "config_paths_validation_and_backup_contract",
            "--nocapture",
        ])
        .env("HYPE_FLOW_CORE_TEST_ROOT", root.path())
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env("HOME", root.path());
    assert!(check.status().unwrap().success());
    check
        .env("XDG_DATA_HOME", "")
        .env("XDG_CONFIG_HOME", "relative");
    assert!(check.status().unwrap().success());
    let xdg = root.path().join("xdg");
    fs::create_dir(&xdg).unwrap();
    check
        .env("XDG_DATA_HOME", &xdg)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("XDG_RUNTIME_DIR", &xdg);
    assert!(check.status().unwrap().success());
}

fn check_config_paths(root: &std::path::Path) {
    if std::env::var_os("XDG_CONFIG_HOME").is_none_or(|value| value == "relative") {
        assert_eq!(config::config_dir(), root.join(".config/hype-flow"));
        assert_eq!(config::data_dir(), root.join(".local/share/hype-flow"));
        assert!(config::runtime_dir().is_err());
        return;
    }
    let directory = root.join("xdg/hype-flow");
    symlink(root, &directory).unwrap();
    assert!(config::runtime_dir().is_err());
    fs::remove_file(&directory).unwrap();
    assert_eq!(config::runtime_dir().unwrap(), directory);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(config::config_dir(), directory);
    assert_eq!(config::data_dir(), directory);

    let defaults = Config::load().unwrap();
    assert_eq!(
        defaults.model,
        directory
            .join("models/parakeet-unified-en-int8")
            .to_string_lossy()
    );
    assert_eq!(defaults.model_variant, "unified");
    assert_eq!(defaults.feature_dim, 128);
    assert_eq!(
        defaults.server,
        directory
            .join("backend/parakeet/recognizer")
            .to_string_lossy()
    );
    assert_eq!(defaults.language, "en");
    assert_eq!(defaults.unload_minutes, 10);
    assert_eq!(defaults.dictation_shortcut, "Alt+Space");
    assert_eq!(defaults.cancel_shortcut, "Mod+Ctrl+Escape");
    assert_eq!(
        (defaults.boost_score, defaults.fuzzy_tolerance),
        (1.5, 0.18)
    );
    assert!(defaults.paste && defaults.auto_correct && defaults.filter_words);

    let path = directory.join("config.toml");
    let backup = directory.join("config.toml.previous");
    fs::write(
        &path,
        "model = \"/test/parakeet-unified-en-int8\"\nmodel_variant = \"parakeet-ctc\"\n",
    )
    .unwrap();
    let switching = Config::load().unwrap();
    assert_eq!(switching.model, "/test/parakeet-unified-en-int8");
    assert_eq!(switching.model_variant, "parakeet-ctc");
    assert_eq!(switching.feature_dim, 128);
    fs::write(&path, "model_variant = \"unified\"\n").unwrap();
    let unified = Config::load().unwrap();
    assert!(unified.model.ends_with("/models/parakeet-unified-en-int8"));
    assert_eq!(unified.feature_dim, 128);
    for (input, field) in [
        ("threads = true\n", "threads"),
        ("unknown = 1\n", "Unknown settings"),
        ("threads = 65", "threads"),
        ("boost_score = \"2\"", "boost_score"),
        ("boost_score = 8", "Vocabulary strength"),
        ("fuzzy_tolerance = nan", "Near-miss tolerance"),
        ("unload_minutes = 7", "Unload"),
        ("unload_minutes = false", "unload_minutes"),
        ("corrections = \"bad\"", "Correction line"),
        ("model_variant = \"remote\"", "model_variant"),
        ("dictation_mode = \"always\"", "dictation_mode"),
        ("confidence_threshold = nan", "confidence_threshold"),
        ("anomaly_threshold = inf", "anomaly_threshold"),
        ("feature_dim = 64", "feature_dim"),
        ("provider = \"cuda\"", "CPU provider"),
    ] {
        fs::write(&path, input).unwrap();
        assert!(
            Config::load().unwrap_err().to_string().contains(field),
            "{input}"
        );
    }
    let original = format!(
        "model = \"{0}/models/ggml-large-v3.bin\"\nserver = \"{0}/backend/whisper-server-linux-x64-vulkan\"\ngpu = true\nprompt = \"Saved names\"\npaste = false\nboost_score = 2\n",
        directory.display(),
    );
    fs::write(&path, &original).unwrap();
    let mut values = Config::load().unwrap();
    assert_eq!(values.model, defaults.model);
    assert_eq!(values.server, defaults.server);
    assert_eq!(values.prompt, "Saved names");
    assert_eq!(values.boost_score, 2.0);
    assert!(!values.paste);
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(Config::from_table(toml::from_str("gpu = false").unwrap()).is_ok());

    values.model = root.join("custom/model").display().to_string();
    values.server = root.join("custom/recognizer").display().to_string();
    values.prompt = "नमस्ते, \"niri\"\nproject".into();
    values.save().unwrap();
    assert_eq!(Config::load().unwrap(), values);
    assert!(!fs::read_to_string(&path).unwrap().contains("gpu"));
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    for saved in [&path, &backup] {
        assert_eq!(
            fs::metadata(saved).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    values.language = "hi".into();
    values.save().unwrap();
    assert_eq!(Config::load().unwrap(), values);
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    let previous = fs::read_to_string(&path).unwrap();
    values.corrections = "missing separator".into();
    assert!(values.save().is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), previous);

    let first = Config {
        model: root.join("model-a").display().to_string(),
        ..Config::load().unwrap()
    };
    let second = Config {
        model: root.join("model-b").display().to_string(),
        ..first.clone()
    };
    let ready = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for (value, ready) in [(&first, &ready), (&second, &ready)] {
            scope.spawn(move || {
                ready.wait();
                value.save().unwrap();
            });
        }
    });
    let current = Config::load().unwrap();
    let backed_up: Config = toml::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
    assert!((current == first && backed_up == second) || (current == second && backed_up == first));

    let target = root.join("nested/real");
    fs::create_dir_all(&target).unwrap();
    symlink(&target, root.join("linked")).unwrap();
    fs::write(
        &path,
        "model = \"~/linked/../new\"\nserver = \"~/linked/missing/../worker\"",
    )
    .unwrap();
    let values = Config::load().unwrap();
    assert_eq!(values.model, root.join("nested/new").to_string_lossy());
    assert_eq!(
        values.server,
        root.join("nested/real/worker").to_string_lossy()
    );

    symlink("cycle", root.join("cycle")).unwrap();
    fs::write(&path, "model = \"~/cycle/model\"").unwrap();
    assert_eq!(
        Config::load().unwrap().model,
        root.join("cycle/model").to_string_lossy()
    );

    fs::write(&path, r#"server = "bad\u0000path""#).unwrap();
    assert!(Config::load().is_err());
}
