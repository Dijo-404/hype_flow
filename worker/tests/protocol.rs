use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_hype-flow-recognizer");
const READY: &[u8] = b"{\"ready\":true}";

fn staged_worker() -> TempDir {
    static LIBRARY: OnceLock<Vec<u8>> = OnceLock::new();
    let library = LIBRARY.get_or_init(|| {
        let build = tempfile::tempdir().unwrap();
        let root = build.path();
        // Use the pinned layouts without the dynamic loader, which the fake supplies.
        let (types, _) = include_str!("../src/bindings.rs")
            .split_once("pub struct SherpaApi {")
            .unwrap();
        fs::write(root.join("types.rs"), types).unwrap();
        fs::write(root.join("fake.rs"), include_str!("fixtures/fake.rs")).unwrap();
        let output = Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .current_dir(root)
            .args([
                "--edition=2024",
                "--crate-type=cdylib",
                "fake.rs",
                "-o",
                "libfake.so",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read(root.join("libfake.so")).unwrap()
    });

    let stage = tempfile::tempdir().unwrap();
    let root = stage.path();
    fs::create_dir(root.join("lib")).unwrap();
    fs::copy(BINARY, root.join("recognizer")).unwrap();
    fs::write(root.join("lib/libsherpa-onnx-c-api.so"), library).unwrap();
    stage
}

fn worker(stage: &TempDir) -> Command {
    let mut command = Command::new(stage.path().join("recognizer"));
    command.env_remove("FAKE_SHERPA_MODE");
    command
}

fn spawn(command: &mut Command) -> std::process::Child {
    // Parallel test children can briefly inherit a staged executable's write FD.
    for _ in 0..5 {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.unwrap(),
        }
    }
    command.spawn().unwrap()
}

fn run(mut command: Command, input: &[u8]) -> Output {
    let mut child = spawn(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn frame(reader: &mut impl Read) -> Vec<u8> {
    let mut length = [0; 4];
    reader.read_exact(&mut length).unwrap();
    let length = u32::from_le_bytes(length) as usize;
    assert!(length <= 1_048_576);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    body
}

fn pcm(samples: &[i16]) -> Vec<u8> {
    let mut bytes = (samples.len() as u32).to_le_bytes().to_vec();
    bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
    bytes
}

fn assert_released(output: &Output, expected: &[&str]) {
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    let resources: Vec<_> = diagnostics
        .lines()
        .filter_map(|line| line.strip_prefix("drop "))
        .collect();
    assert_eq!(resources, expected, "{diagnostics}");
}

#[test]
fn invalid_options_fail_before_loading_native_library() {
    for args in [
        &[][..],
        &["model"],
        &["model", "0"],
        &["model", "65"],
        &["model", "2 "],
        &["model", "2", "--unknown"],
        &["model", "2", "--hotwords", "words"],
        &["model", "2", "--hotwords", "words", "NaN"],
        &["model", "2", "--hotwords", "words", "3.1"],
        &["model", "2", "--feature-dim", "79"],
        &["model", "2", "--feature-dim", "129"],
        &["model", "2", "--provider", "cuda"],
        &["model", "2", "--provider", "cpu:/etc/passwd"],
        &["model", "2", "--vad"],
        &["model", "2", "--denoise"],
    ] {
        let output = Command::new(BINARY).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(64), "{args:?}");
        assert!(output.stdout.is_empty());
    }
    let output = Command::new(BINARY)
        .args(["x".repeat(4096), "2".into()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn malformed_requests_and_clean_eof_have_distinct_statuses() {
    let stage = staged_worker();
    for (input, status) in [
        (&[][..], 0),
        (&[1][..], 64),
        (&[1, 0, 0][..], 64),
        (&0_u32.to_le_bytes(), 64),
        (&4_800_000_u32.to_le_bytes(), 1),
        (&4_800_001_u32.to_le_bytes(), 64),
        (&[2, 0, 0, 0, 1, 2, 3][..], 1),
    ] {
        let mut command = worker(&stage);
        command.args(["model", "2", "--vad", "vad", "--denoise", "denoise"]);
        let output = run(command, input);
        assert_eq!(output.status.code(), Some(status), "{input:?}");
        let mut stdout = output.stdout.as_slice();
        assert_eq!(frame(&mut stdout), READY);
        assert!(stdout.is_empty());
        assert_released(&output, &["vad", "denoiser", "recognizer"]);
    }
}

#[test]
fn readiness_and_results_flush_before_eof_and_pcm_is_signed_little_endian() {
    let stage = staged_worker();
    let (mut stdout, child_stdout) = UnixStream::pair().unwrap();
    stdout
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut child = spawn(
        worker(&stage)
            .args(["model", "  +2"])
            .stdin(Stdio::piped())
            .stdout(OwnedFd::from(child_stdout))
            .stderr(Stdio::piped()),
    );
    let mut stdin = child.stdin.take().unwrap();
    assert_eq!(frame(&mut stdout), READY);
    for (samples, expected) in [
        (
            [-32768, 32767, 0],
            b"{\"samples\":[-1.0, 0.9999695, 0.0]}".as_slice(),
        ),
        (
            [16384, -16384, 1],
            b"{\"samples\":[0.5, -0.5, 3.0517578e-5]}".as_slice(),
        ),
    ] {
        for part in pcm(&samples).chunks(3) {
            stdin.write_all(part).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(frame(&mut stdout), expected);
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_released(
        &output,
        &["result", "stream", "result", "stream", "recognizer"],
    );
}

#[test]
fn boosted_denoised_audio_is_windowed_and_overlapping_padding_is_deduplicated() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.args([
        "model",
        "2",
        "--hotwords",
        "words",
        "0x1.8p0",
        "--vad",
        "vad",
        "--denoise",
        "denoise",
    ]);
    let output = run(command, &pcm(&(0..20_000).collect::<Vec<_>>()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    assert_eq!(
        frame(&mut stdout),
        b"{\"count\":14100,\"first\":0.04272461,\"last\":0.25785828}"
    );
    assert!(stdout.is_empty());
    assert_released(
        &output,
        &[
            "segment",
            "segment",
            "audio",
            "result",
            "stream",
            "vad",
            "denoiser",
            "recognizer",
        ],
    );
}

#[test]
fn ctc_keeps_original_samples_between_vad_ranges() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.env("FAKE_SHERPA_MODE", "ctc-pause").args([
        "model",
        "2",
        "--ctc",
        "--feature-dim",
        "80",
        "--vad",
        "vad",
        "--denoise",
        "denoise",
    ]);
    let output = run(command, &pcm(&(0..20_000).collect::<Vec<_>>()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    let result: serde_json::Value = serde_json::from_slice(&frame(&mut stdout)).unwrap();
    assert_eq!(result["text"], "paused");
    assert_eq!(result["timestamps"], serde_json::json!([0.75]));
}

#[test]
fn empty_vad_returns_empty_even_when_denoiser_is_incompatible() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.env("FAKE_SHERPA_MODE", "fallback").args([
        "model",
        "2",
        "--vad",
        "vad",
        "--denoise",
        "denoise",
    ]);
    let output = run(command, &pcm(&[-32768, 32767, 0]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    assert_eq!(frame(&mut stdout), b"{\"text\":\"\"}");
    assert!(stdout.is_empty());
    assert_released(&output, &["audio", "vad", "denoiser", "recognizer"]);
}

#[test]
fn native_initialization_failure_does_not_send_ready_and_cleans_up() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.env("FAKE_SHERPA_MODE", "denoiser-fails").args([
        "model",
        "2",
        "--vad",
        "vad",
        "--denoise",
        "denoise",
    ]);
    let output = run(command, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_released(&output, &["vad", "recognizer"]);
}

#[test]
fn oversized_native_result_releases_result_and_stream() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command
        .env("FAKE_SHERPA_MODE", "oversize-result")
        .args(["model", "2"]);
    let output = run(command, &pcm(&[0]));
    assert_eq!(output.status.code(), Some(1));
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    assert!(stdout.is_empty());
    assert_released(&output, &["result", "stream", "recognizer"]);
}

#[test]
fn warmup_decode_finishes_before_ready_and_is_not_returned_as_a_transcript() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.args(["model", "2", "--warmup"]);
    let output = run(command, &pcm(&[16384, -16384, 1]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    assert_eq!(
        frame(&mut stdout),
        b"{\"samples\":[0.5, -0.5, 3.0517578e-5]}"
    );
    assert!(stdout.is_empty());
    assert_released(
        &output,
        &["result", "stream", "result", "stream", "recognizer"],
    );
}

#[test]
fn gain_only_mode_passes_normalized_pcm_to_recognizer() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command.args(["model", "2", "--normalize-gain"]);
    let output = run(command, &pcm(&[32, -32, 0]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    assert_eq!(frame(&mut stdout), READY);
    assert_eq!(
        frame(&mut stdout),
        b"{\"samples\":[0.009765625, -0.009765625, 0.0]}"
    );
    assert!(stdout.is_empty());
    assert_released(&output, &["result", "stream", "recognizer"]);
}

#[test]
fn audible_empty_vad_can_retry_once_but_default_still_rejects_it() {
    for retry in [false, true] {
        let stage = staged_worker();
        let mut command = worker(&stage);
        command
            .env("FAKE_SHERPA_MODE", "energy-retry")
            .args(["model", "2", "--vad", "vad"]);
        if retry {
            command.arg("--soft-speech-retry");
        }
        let output = run(command, &pcm(&vec![1000; 1600]));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut stdout = output.stdout.as_slice();
        assert_eq!(frame(&mut stdout), READY);
        let result = frame(&mut stdout);
        if retry {
            assert!(String::from_utf8_lossy(&result).contains("\"count\":1600"));
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .matches("drop stream")
                    .count(),
                1
            );
        } else {
            assert_eq!(result, b"{\"text\":\"\"}");
        }
    }
}

#[test]
fn low_memory_model_can_select_eighty_feature_bins() {
    let stage = staged_worker();
    let mut command = worker(&stage);
    command
        .env("FAKE_SHERPA_MODE", "feature-80")
        .args(["model", "2", "--feature-dim", "80"]);
    let output = run(command, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(frame(&mut output.stdout.as_slice()), READY);
}

#[test]
fn audible_empty_recognition_retries_exactly_once() {
    for retry in [false, true] {
        let stage = staged_worker();
        let mut command = worker(&stage);
        command
            .env("FAKE_SHERPA_MODE", "empty-first")
            .args(["model", "2"]);
        if retry {
            command.arg("--soft-speech-retry");
        }
        let output = run(command, &pcm(&vec![1000; 1600]));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut stdout = output.stdout.as_slice();
        assert_eq!(frame(&mut stdout), READY);
        let result = frame(&mut stdout);
        assert_eq!(
            result,
            if retry {
                &b"{\"text\":\"recovered\"}"[..]
            } else {
                &b"{\"text\":\"\"}"[..]
            }
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("drop stream")
                .count(),
            if retry { 2 } else { 1 }
        );
    }
}
