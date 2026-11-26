use hype_flow::audio::Recorder;
use hype_flow::backend::{Engine, MAX_SECONDS, RATE, read_wav, wav_bytes};
use hype_flow::config::Config;
use hype_flow::process::run_command;
use hype_flow::setup::MODEL_FILES;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(Instant::now() < deadline, "{}", path.display());
        thread::sleep(Duration::from_millis(10));
    }
}

fn events(temp: &TempDir) -> UnixDatagram {
    let events = UnixDatagram::bind(temp.path().join("events")).unwrap();
    events
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    events
}

fn event(events: &UnixDatagram) -> String {
    let mut buffer = [0; 4096];
    let length = events.recv(&mut buffer).unwrap();
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

fn worker_is_running(path: &Path) -> bool {
    fs::read_dir("/proc").unwrap().flatten().any(|entry| {
        entry.file_name().to_string_lossy().parse::<u32>().is_ok()
            && fs::read(entry.path().join("cmdline")).is_ok_and(|command| {
                command
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == path.as_os_str().as_bytes())
            })
    })
}

#[test]
fn command_drains_both_outputs_while_writing_large_input() {
    let input = vec![51; 1024 * 1024];
    let output = run_command(
        Command::new("python3").args([
            "-c", "import sys; sys.stderr.buffer.write(b'x' * 65536); sys.stderr.flush(); sys.stdout.buffer.write(sys.stdin.buffer.read())",
        ]),
        Some(&input),
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, input);
    assert_eq!(output.stderr, vec![b'x'; 65536]);
}

#[test]
fn command_waits_for_exit_after_both_outputs_close() {
    let output = run_command(
        Command::new("sh").args(["-c", "exec 1>&- 2>&-; sleep 0.03; exit 7"]),
        None,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
#[ignore = "requires a real PipeWire session; captures one frame and discards audio"]
fn real_pipewire_delivers_first_frame_and_closes() {
    let (tx, rx) = mpsc::channel();
    let began = Instant::now();
    let recorder = Recorder::start(
        "auto",
        false,
        move |_, seconds| {
            let _ = tx.send(seconds);
        },
        || {},
    )
    .unwrap();
    let seconds = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let elapsed = began.elapsed();
    let pcm = recorder.stop().unwrap();
    assert!(seconds > 0.0 && !pcm.is_empty() && pcm.len().is_multiple_of(2));
    eprintln!(
        "Real PipeWire first frame: {} ms; discarded {} PCM bytes",
        elapsed.as_millis(),
        pcm.len()
    );
}

#[test]
fn command_timeout_reaps_the_process_even_if_its_pipes_are_blocked() {
    let temp = TempDir::new().unwrap();
    let pid_path = temp.path().join("pid");
    let start = Instant::now();
    let result = run_command(
        Command::new("sh")
            .args([
                "-c",
                "sleep 60 & echo \"$$ $!\" > \"$1\"; exec sleep 60",
                "sh",
            ])
            .arg(&pid_path),
        Some(&vec![1; 1024 * 1024]),
        Duration::from_millis(450),
    );
    assert!(result.unwrap_err().to_string().contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(3));
    let pids = fs::read_to_string(pid_path).unwrap();
    let (parent, descendant) = pids.trim().split_once(' ').unwrap();
    assert!(!Path::new("/proc").join(parent).exists());
    let descendant = Path::new("/proc").join(descendant).join("stat");
    let deadline = Instant::now() + Duration::from_secs(1);
    while let Ok(stat) = fs::read_to_string(&descendant) {
        if stat.rsplit_once(") ").unwrap().1.starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Descendant survived group termination"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn command_output_is_bounded() {
    let result = run_command(&mut Command::new("yes"), None, Duration::from_secs(5));
    assert!(result.unwrap_err().to_string().contains("output limit"));
}

fn fake_config(script: &str) -> (TempDir, Config) {
    let temp = TempDir::new().unwrap();
    let model = temp.path().join("model");
    let server = temp.path().join("worker");
    fs::create_dir(&model).unwrap();
    for file in MODEL_FILES {
        fs::write(model.join(file), b"fixture").unwrap();
    }
    executable(&server, &format!("#!/usr/bin/python3\n{script}"));
    let config = Config {
        model: model.to_string_lossy().into_owned(),
        server: server.to_string_lossy().into_owned(),
        cut_silence: false,
        ..Config::default()
    };
    (temp, config)
}

fn fake_engine(script: &str) -> (TempDir, Engine) {
    let (temp, config) = fake_config(script);
    let engine = Engine::new(config, temp.path().to_owned());
    (temp, engine)
}

const WORKER_SETUP: &str = r#"
import json, os, pathlib, socket, struct, sys, time
root = pathlib.Path(sys.argv[1]).parent
def event(text):
    # The sandbox makes files read-only, so report to the test over a socket.
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
        channel.sendto(text.encode(), str(root / 'events'))
def send(value):
    body = json.dumps(value).encode()
    for part in (struct.pack('<I', len(body))[:2], struct.pack('<I', len(body))[2:], body[:3], body[3:]):
        os.write(1, part)
        time.sleep(.005)
def read_pcm():
    head = sys.stdin.buffer.read(4)
    if not head:
        return None
    count, = struct.unpack('<I', head)
    payload = sys.stdin.buffer.read(count * 2)
    assert len(payload) == count * 2
    return payload
"#;

#[test]
fn worker_load_is_shared_and_pcm_framing_handles_partial_transfers() {
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
event('started')
send({'ready': True})
while (payload := read_pcm()) is not None:
    assert payload == b'\x34\x12' * 130000
    send({'text': '  recognized\n words. '})
"#
    );
    let (temp, engine) = fake_engine(&script);
    let events = events(&temp);
    thread::scope(|scope| {
        let first = scope.spawn(|| engine.ensure());
        let second = scope.spawn(|| engine.ensure());
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
    });
    assert_eq!(event(&events), "started");
    events.set_nonblocking(true).unwrap();
    assert_eq!(
        events.recv(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        engine.transcribe(&[0x34, 0x12].repeat(130000)).unwrap(),
        "recognized words."
    );
    assert_eq!(
        fs::metadata(temp.path().join("backend.log"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn detailed_transcription_keeps_duration_of_skipped_silent_tail() {
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
send({'ready': True})
while (payload := read_pcm()) is not None:
    assert any(payload)
    event(str(len(payload) // 32000))
    send({'text': 'speech', 'tokens': [' speech'], 'timestamps': [0.0],
          'durations': [1.0], 'ys_log_probs': [-0.1]})
"#
    );
    let (temp, engine) = fake_engine(&script);
    let events = events(&temp);
    engine.ensure().unwrap();
    let mut pcm = [1, 0].repeat(15 * RATE);
    pcm.resize(35 * RATE * 2, 0);
    let mut transcript = engine.transcribe_detailed(&pcm).unwrap();
    assert_eq!(transcript.text, "speech");
    assert_eq!(transcript.audio_seconds, 35.0);
    assert_eq!(event(&events), "15");
    let next = engine.transcribe_detailed(&[1, 0].repeat(RATE)).unwrap();
    assert_eq!(event(&events), "1");
    transcript.append(next, transcript.audio_seconds);
    assert_eq!(transcript.audio_seconds, 36.0);
    assert_eq!(transcript.words[1].start_seconds, 35.0);
}

#[test]
fn ctc_normalizes_leading_silence_before_splitting_and_keeps_source_time() {
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
assert '--ctc' in sys.argv and '--vad' in sys.argv
send({'ready': True})
assert read_pcm() == bytes(1024) + b'\x00\x01' * 239488
send({'text': 'speech', 'tokens': [' speech'], 'timestamps': [0.25],
      'durations': [0.5], 'ys_log_probs': [-0.1]})
assert read_pcm() == bytes(1024) + b'\x00\x01' * 239488
send({'text': 'speech', 'tokens': [' speech'], 'timestamps': [0.01],
      'durations': [0.5], 'ys_log_probs': [-0.1]})
assert read_pcm() is None
"#
    );
    let (temp, mut config) = fake_config(&script);
    let model = temp.path().join("parakeet-110m-ctc-en-int8");
    fs::create_dir(&model).unwrap();
    for path in [
        model.join("model.int8.onnx"),
        model.join("tokens.txt"),
        temp.path().join("silero_vad.onnx"),
    ] {
        fs::write(path, b"fixture").unwrap();
    }
    config.model = model.to_string_lossy().into_owned();
    config.cut_silence = true;
    let engine = Engine::new(config, temp.path().to_owned());
    engine.ensure().unwrap();
    let mut pcm = vec![0; 3 * RATE * 2];
    pcm.extend([0, 1].repeat(239_488));
    let transcript = engine.transcribe_detailed(&pcm).unwrap();
    assert_eq!(transcript.text, "speech");
    assert!((transcript.audio_seconds - 17.968).abs() < 1e-9);
    assert!((transcript.words[0].start_seconds - 3.218).abs() < 1e-9);
    let unpadded = engine.transcribe_detailed(&pcm[3 * RATE * 2..]).unwrap();
    assert_eq!(unpadded.text, transcript.text);
    assert!((unpadded.audio_seconds - 14.968).abs() < 1e-9);
    assert_eq!(unpadded.words[0].start_seconds, 0.0);
}

#[test]
fn worker_flags_use_private_vocabulary_and_remove_it_on_stop() {
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
assert sys.argv[2] == '3'
args = sys.argv[3:]
assert args[:11] == ['--warmup', '--blank-penalty', '0', '--max-active-paths', '4', '--vad-threshold', '0.3', '--vad-min-silence', '0.45', '--max-piece-seconds', '15']
assert args[11] == '--hotwords'
hotwords = pathlib.Path(args[12])
assert hotwords.read_text() == 'Niri\nniri'
assert hotwords.stat().st_mode & 0o777 == 0o600
assert args[13:] == ['1.8', '--vad', str(root / 'silero_vad.onnx'),
                        '--denoise', str(root / 'gtcrn_simple.onnx')]
event(str(hotwords))
send({'ready': True})
read_pcm()
"#
    );
    let (temp, mut config) = fake_config(&script);
    config.threads = 3;
    config.feature_dim = 128;
    config.vocabulary_boosting = true;
    config.prompt = "Niri".into();
    config.boost_score = 1.8;
    config.cut_silence = true;
    config.noise_suppression = true;
    for file in ["model/bpe.vocab", "silero_vad.onnx", "gtcrn_simple.onnx"] {
        fs::write(temp.path().join(file), "fixture").unwrap();
    }
    let engine = Engine::new(config, temp.path().to_owned());
    let events = events(&temp);
    engine.ensure().unwrap_or_else(|error| {
        let log = fs::read_to_string(temp.path().join("backend.log")).unwrap_or_default();
        panic!("{error:#}\n{log}");
    });
    let hotwords = event(&events);
    assert!(Path::new(&hotwords).is_file());
    engine.stop();
    assert!(!Path::new(&hotwords).exists());
}

#[test]
fn sandboxed_worker_cannot_write_files_or_reach_session_sockets() {
    let outside = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let hidden = TempDir::new().unwrap();
    let script = format!(
        "{WORKER_SETUP}\noutside, hidden = {:?}, {:?}\n{}",
        outside.path().join("escape"),
        hidden.path(),
        r#"
import errno
def attempt(path):
    try:
        pathlib.Path(path).write_text('escaped')
        return 'written'
    except OSError as error:
        return errno.errorcode[error.errno]
shared = os.environ.get('XDG_RUNTIME_DIR', '')
print('worker log is writable', file=sys.stderr, flush=True)
event(json.dumps({
    'runtime': attempt(root / 'escape'),
    'outside': attempt(outside),
    'hidden': os.path.exists(hidden),
    'shared': os.listdir(shared) if os.path.isabs(shared) and os.path.isdir(shared) else [],
}))
send({'ready': True})
read_pcm()
"#
    );
    let (temp, engine) = fake_engine(&script);
    let events = events(&temp);
    engine.ensure().unwrap();
    let report: serde_json::Value = serde_json::from_str(&event(&events)).unwrap();
    let log = fs::read_to_string(temp.path().join("backend.log")).unwrap();
    assert!(log.contains("worker log is writable"), "{log}");
    if !log.contains("bubblewrap") {
        eprintln!("bubblewrap is unavailable; sandbox checks skipped");
        return;
    }
    assert_eq!(
        report,
        serde_json::json!({"runtime": "EROFS", "outside": "EROFS", "hidden": false, "shared": []})
    );
}

#[test]
fn worker_rejects_empty_oversize_non_object_and_missing_text_responses() {
    for (send, expected) in [
        ("os.write(1, struct.pack('<I', 0))", "response size"),
        ("os.write(1, struct.pack('<I', 1048577))", "response size"),
        ("send([])", "Invalid speech backend response"),
        ("send({'other': 1})", "no transcript"),
        ("sys.exit(1)", "backend exited"),
    ] {
        let (_temp, engine) = fake_engine(&format!(
            "{WORKER_SETUP}\nsend({{'ready': True}})\nread_pcm()\n{send}\n"
        ));
        engine.ensure().unwrap();
        let error = engine.transcribe(&[1, 0]).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[test]
fn silence_never_needs_a_worker_and_invalid_lengths_fail() {
    let temp = TempDir::new().unwrap();
    let engine = Engine::new(Config::default(), temp.path().to_owned());
    assert_eq!(engine.transcribe(&[0; 400]).unwrap(), "");
    assert!(engine.transcribe(&[1]).is_err());
    assert!(
        engine
            .transcribe(&vec![0; MAX_SECONDS * RATE * 2 + 2])
            .is_err()
    );
}

#[test]
fn cancellation_of_blocked_worker_io_reaps_worker_and_allows_restart() {
    for read_request in ["read_pcm()", "pass"] {
        let script = format!(
            "{WORKER_SETUP}\n{}",
            r#"
first = not (root / 'restarted').exists()
send({'ready': True})
if first:
    READ_REQUEST
    time.sleep(60)
else:
    read_pcm()
    send({'text': 'new generation'})
"#
            .replace("READ_REQUEST", read_request)
        );
        let (temp, engine) = fake_engine(&script);
        engine.ensure().unwrap();
        fs::write(temp.path().join("restarted"), "").unwrap();
        let worker = temp.path().join("worker");
        assert!(worker_is_running(&worker));
        let engine = Arc::new(engine);
        let pending = {
            let engine = engine.clone();
            thread::spawn(move || engine.transcribe(&[1, 0].repeat(130000)))
        };
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        engine.stop();
        assert!(
            pending
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancel")
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(!worker_is_running(&worker));
        assert!(engine.ensure().is_err());
        engine.prepare();
        engine.ensure().unwrap();
        assert_eq!(engine.transcribe(&[1, 0]).unwrap(), "new generation");
    }
}

#[test]
fn cancelled_startup_during_handshake_reaps_worker() {
    let (temp, engine) = fake_engine(&format!(
        "{WORKER_SETUP}\nevent('started')\ntime.sleep(60)\n"
    ));
    let events = events(&temp);
    let worker = temp.path().join("worker");
    thread::scope(|scope| {
        let startup = scope.spawn(|| engine.ensure());
        assert_eq!(event(&events), "started");
        assert!(worker_is_running(&worker));
        let start = Instant::now();
        engine.stop();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(format!("{:#}", startup.join().unwrap().unwrap_err()).contains("cancelled"));
    });
    assert!(!worker_is_running(&worker));
}

#[test]
fn background_stop_keeps_a_restarted_worker_running() {
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
if not (root / 'restarted').exists():
    event('starting')
    time.sleep(60)
else:
    send({'ready': True})
    while read_pcm() is not None:
        send({'text': 'new generation'})
"#
    );
    let (temp, engine) = fake_engine(&script);
    let events = events(&temp);
    let engine = Arc::new(engine);
    thread::scope(|scope| {
        let startup = scope.spawn(|| engine.ensure());
        assert_eq!(event(&events), "starting");
        fs::write(temp.path().join("restarted"), "").unwrap();
        engine.stop_in_background();
        engine.prepare();
        engine.ensure().unwrap();
        assert!(startup.join().unwrap().is_err());
    });
    assert_eq!(engine.transcribe(&[1, 0]).unwrap(), "new generation");
}

#[test]
fn wav_accepts_mono_pcm16_and_rejects_a_different_format() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("recording.wav");
    let pcm = [1, 0, 0, 128, 255, 127];
    fs::write(&path, wav_bytes(&pcm)).unwrap();
    assert_eq!(read_wav(&path).unwrap(), pcm);
    let mut wav = wav_bytes(&pcm);
    wav[22] = 2;
    fs::write(&path, wav).unwrap();
    assert!(read_wav(&path).is_err());
}

#[test]
fn capture_reassembles_frames_and_caps_storage_and_reports_exit_once() {
    if std::env::var_os("HYPE_RUNTIME_CAPTURE_FIXTURE").is_none() {
        let temp = TempDir::new().unwrap();
        executable(
            &temp.path().join("pw-record"),
            r#"#!/usr/bin/python3
import os, pathlib, sys, time
mode = sys.argv[sys.argv.index('--target') + 1]
root = pathlib.Path(__file__).parent
if mode == 'cap':
    os.write(1, b'\x00')
    os.write(1, b'\x40' + b'\x00\x40' * 4800100)
elif mode == 'exit':
    sys.stderr.write('selected device disappeared\n')
    sys.exit(1)
elif mode == 'held':
    with (root / 'held').open('a') as pid:
        pid.write(str(os.getpid()) + '\n')
    while True:
        os.write(1, b'\x00\x40' * 800)
        time.sleep(.01)
"#,
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capture_reassembles_frames_and_caps_storage_and_reports_exit_once",
                "--nocapture",
            ])
            .env("HYPE_RUNTIME_CAPTURE_FIXTURE", temp.path())
            .env("PATH", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let (end, ended) = mpsc::channel();
    let (frame, frames) = mpsc::channel();
    let recorder = Recorder::start(
        "cap",
        false,
        move |volume, duration| {
            if duration == 0.05 {
                let _ = frame.send(volume);
            }
        },
        move || {
            let _ = end.send(());
        },
    )
    .unwrap();
    assert_eq!(frames.recv_timeout(Duration::from_secs(5)).unwrap(), 1.0);
    ended.recv_timeout(Duration::from_secs(5)).unwrap();
    let audio = recorder.stop().unwrap();
    assert_eq!(audio.len(), 9_600_000);
    assert!(
        audio
            .as_chunks::<2>()
            .0
            .iter()
            .all(|sample| *sample == [0, 64])
    );
    assert!(ended.try_recv().is_err());

    let (end, ended) = mpsc::channel();
    let recorder = Recorder::start(
        "exit",
        false,
        |_, _| {},
        move || {
            let _ = end.send(());
        },
    )
    .unwrap();
    ended.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(format!("{:#}", recorder.stop().unwrap_err()).contains("selected device disappeared"));
    assert!(ended.try_recv().is_err());

    let (frame, frames) = mpsc::channel();
    let recorder = Recorder::start(
        "held",
        true,
        move |_, _| {
            let _ = frame.send(());
        },
        || {},
    )
    .unwrap();
    frames.recv_timeout(Duration::from_secs(5)).unwrap();
    recorder.stop().unwrap();
    thread::sleep(Duration::from_millis(300));
    let recorder = Recorder::start("held", true, |_, _| {}, || {}).unwrap();
    let audio = recorder.stop().unwrap();
    assert!(audio.len() >= 16_000, "{}", audio.len());
    hype_flow::audio::release();
    let root = std::env::var_os("HYPE_RUNTIME_CAPTURE_FIXTURE").unwrap();
    let pids = fs::read_to_string(Path::new(&root).join("held")).unwrap();
    assert_eq!(pids.lines().count(), 1);
    assert!(!Path::new("/proc").join(pids.trim()).exists());
}

#[test]
fn eager_capture_decodes_before_stop_and_preserves_all_pcm() {
    if std::env::var_os("HYPE_EAGER_FIXTURE").is_none() {
        let temp = TempDir::new().unwrap();
        executable(
            &temp.path().join("pw-record"),
            r#"#!/usr/bin/python3
import os, pathlib, time
root = pathlib.Path(__file__).parent
seconds = 21 if 'long' in __import__('sys').argv else 6
os.write(1, b'\x00\x40' * (16000 * seconds))
while not (root / 'decoded').exists():
    time.sleep(.01)
os.write(1, b'\x00\x20' * 16000)
(root / 'captured').write_text('yes')
time.sleep(30)
"#,
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "eager_capture_decodes_before_stop_and_preserves_all_pcm",
                "--nocapture",
            ])
            .env("HYPE_EAGER_FIXTURE", temp.path())
            .env("PATH", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let capture_root = std::env::var_os("HYPE_EAGER_FIXTURE").unwrap();
    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
send({'ready': True})
while (payload := read_pcm()) is not None:
    with (root / 'received').open('ab') as audio:
        audio.write(payload)
    pathlib.Path(os.environ['HYPE_EAGER_FIXTURE'], 'decoded').write_text('yes')
    send({'text': 'piece'})
"#
    );
    let (temp, engine) = fake_engine(&script);
    let engine = Arc::new(engine);
    let (tx, rx) = mpsc::channel();
    let spool = temp.path().join("recording.pcm");
    let recorder = Recorder::start_eager_spooled(
        "auto",
        false,
        move |_, duration| {
            let _ = tx.send(duration);
        },
        || {},
        engine.clone(),
        5,
        Some(&spool),
    )
    .unwrap();
    wait_for_file(&Path::new(&capture_root).join("captured"));
    while rx.recv_timeout(Duration::from_secs(5)).unwrap() < 7.0 {}
    // The capture process withheld its last second until inference finished.
    assert_eq!(
        fs::metadata(temp.path().join("received")).unwrap().len(),
        (5 * RATE * 2) as u64
    );
    let (tail, committed, recorded) = recorder.stop_eager(|| {}).unwrap();
    assert_eq!(committed, "piece");
    assert_eq!(recorded, 7 * RATE * 2);
    let mut combined = fs::read(temp.path().join("received")).unwrap();
    combined.extend(tail);
    assert_eq!(
        combined,
        [[0, 64].repeat(6 * RATE), [0, 32].repeat(RATE)].concat()
    );
    assert_eq!(fs::read(&spool).unwrap(), combined);
    assert_eq!(fs::metadata(&spool).unwrap().mode() & 0o777, 0o600);
    assert!(engine.peak_worker_rss_kib().unwrap() > 0);

    let script = format!(
        "{WORKER_SETUP}\n{}",
        r#"
first = not (root / 'blocked').exists()
send({'ready': True})
payload = read_pcm()
if first:
    (root / 'blocked').write_text('yes')
    time.sleep(60)
else:
    assert payload == b'\x34\x12' * 16000
    send({'text': 'fresh'})
"#
    );
    let (blocked_root, blocked) = fake_engine(&script);
    let blocked = Arc::new(blocked);
    let recorder =
        Recorder::start_eager("long", false, |_, _| {}, || {}, blocked.clone(), 5).unwrap();
    wait_for_file(&blocked_root.path().join("blocked"));
    let began = Instant::now();
    recorder.cancel_eager();
    let reaper = blocked.stop_in_background();
    blocked.prepare();
    blocked.ensure().unwrap();
    assert_eq!(
        blocked.transcribe(&[0x34, 0x12].repeat(16000)).unwrap(),
        "fresh"
    );
    reaper.join().unwrap();
    assert!(recorder.stop_eager(|| {}).is_err());
    assert!(began.elapsed() < Duration::from_secs(2));
}
