#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

include!("types.rs");

use std::ffi::{CStr, CString};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

fn mode(name: &str) -> bool {
    std::env::var("FAKE_SHERPA_MODE").as_deref() == Ok(name)
}

fn handle<T>() -> *const T {
    Box::into_raw(Box::new(0_u8)).cast()
}

unsafe fn free_handle<T>(pointer: *const T, name: &str) {
    drop(Box::from_raw(pointer.cast_mut().cast::<u8>()));
    eprintln!("drop {name}");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxCreateOfflineRecognizer(
    config: *const SherpaOnnxOfflineRecognizerConfig,
) -> *const SherpaOnnxOfflineRecognizer {
    let config = &*config;
    let model = &config.model_config;
    assert_eq!(config.feat_config.sample_rate, 16000);
    assert_eq!(
        config.feat_config.feature_dim,
        if mode("feature-80") { 80 } else { 128 }
    );
    assert_eq!(model.num_threads, 2);
    assert_eq!(CStr::from_ptr(model.provider), c"cpu");
    assert_eq!(CStr::from_ptr(model.model_type), c"nemo_transducer");
    assert_eq!(
        CStr::from_ptr(model.transducer.encoder),
        c"model/encoder.int8.onnx"
    );
    assert_eq!(
        CStr::from_ptr(model.transducer.decoder),
        c"model/decoder.int8.onnx"
    );
    assert_eq!(
        CStr::from_ptr(model.transducer.joiner),
        c"model/joiner.int8.onnx"
    );
    assert_eq!(CStr::from_ptr(model.tokens), c"model/tokens.txt");
    if config.hotwords_file.is_null() {
        assert_eq!(CStr::from_ptr(config.decoding_method), c"greedy_search");
        assert!(model.bpe_vocab.is_null());
    } else {
        assert_eq!(
            CStr::from_ptr(config.decoding_method),
            c"modified_beam_search"
        );
        assert_eq!(CStr::from_ptr(config.hotwords_file), c"words");
        assert_eq!(CStr::from_ptr(model.modeling_unit), c"bpe");
        assert_eq!(CStr::from_ptr(model.bpe_vocab), c"model/bpe.vocab");
        assert_eq!(config.hotwords_score, 1.5);
        assert_eq!(config.max_active_paths, 4);
    }
    handle()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyOfflineRecognizer(p: *const SherpaOnnxOfflineRecognizer) {
    free_handle(p, "recognizer");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxCreateOfflineStream(
    _: *const SherpaOnnxOfflineRecognizer,
) -> *const SherpaOnnxOfflineStream {
    Box::into_raw(Box::new(Vec::<f32>::new())).cast()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyOfflineStream(p: *const SherpaOnnxOfflineStream) {
    drop(Box::from_raw(p.cast_mut().cast::<Vec<f32>>()));
    eprintln!("drop stream");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxAcceptWaveformOffline(
    p: *const SherpaOnnxOfflineStream,
    rate: i32,
    samples: *const f32,
    n: i32,
) {
    assert_eq!(rate, 16_000);
    *p.cast_mut().cast::<Vec<f32>>() = slice::from_raw_parts(samples, n as usize).to_vec();
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDecodeOfflineStream(
    _: *const SherpaOnnxOfflineRecognizer,
    _: *const SherpaOnnxOfflineStream,
) {
    assert_eq!(PENDING_AUDIO.load(Relaxed), 0);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxGetOfflineStreamResult(
    p: *const SherpaOnnxOfflineStream,
) -> *const SherpaOnnxOfflineRecognizerResult {
    let samples = &*p.cast::<Vec<f32>>();
    static RESULTS: AtomicUsize = AtomicUsize::new(0);
    let json = if mode("empty-first") {
        if RESULTS.fetch_add(1, Relaxed) == 0 {
            "{\"text\":\"\"}".to_owned()
        } else {
            "{\"text\":\"recovered\"}".to_owned()
        }
    } else if mode("oversize-result") {
        "x".repeat(1_048_577)
    } else if samples.len() > 3 {
        format!(
            "{{\"count\":{},\"first\":{:?},\"last\":{:?}}}",
            samples.len(),
            samples[0],
            samples[samples.len() - 1]
        )
    } else {
        format!("{{\"samples\":{samples:?}}}")
    };
    Box::into_raw(Box::new(SherpaOnnxOfflineRecognizerResult {
        json: CString::new(json).unwrap().into_raw(),
        ..Default::default()
    }))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyOfflineRecognizerResult(
    p: *const SherpaOnnxOfflineRecognizerResult,
) {
    let result = Box::from_raw(p.cast_mut());
    drop(CString::from_raw(result.json.cast_mut()));
    eprintln!("drop result");
}

static WINDOWS: AtomicUsize = AtomicUsize::new(0);
static SEGMENTS: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxCreateVoiceActivityDetector(
    config: *const SherpaOnnxVadModelConfig,
    buffer_seconds: f32,
) -> *const SherpaOnnxVoiceActivityDetector {
    let config = &*config;
    let silero = &config.silero_vad;
    assert_eq!(CStr::from_ptr(silero.model), c"vad");
    assert_eq!(CStr::from_ptr(config.provider), c"cpu");
    assert_eq!(config.sample_rate, 16000);
    assert_eq!(config.num_threads, 1);
    assert_eq!(silero.threshold, 0.3);
    assert_eq!(silero.min_silence_duration, 0.45);
    assert_eq!(silero.min_speech_duration, 0.06);
    assert_eq!(silero.window_size, 512);
    assert_eq!(silero.max_speech_duration, 60.0);
    assert_eq!(buffer_seconds, 65.0);
    handle()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyVoiceActivityDetector(
    p: *const SherpaOnnxVoiceActivityDetector,
) {
    free_handle(p, "vad");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorAcceptWaveform(
    _: *const SherpaOnnxVoiceActivityDetector,
    samples: *const f32,
    n: i32,
) {
    let offset = WINDOWS.fetch_add(1, Relaxed);
    if mode("energy-retry") {
        assert_eq!(n, 512.min(1600 - offset * 512) as i32);
        assert!(*samples > 0.02);
    } else if mode("fallback") {
        assert_eq!(n, 3);
        assert_eq!(*samples, -1.0);
    } else {
        assert_eq!(n, if offset == 39 { 32 } else { 512 });
        assert_eq!(*samples, (offset * 512) as f32 / 65536.0);
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorEmpty(
    _: *const SherpaOnnxVoiceActivityDetector,
) -> i32 {
    i32::from(SEGMENTS.load(Relaxed) == 0)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorPop(_: *const SherpaOnnxVoiceActivityDetector) {
    SEGMENTS.fetch_sub(1, Relaxed);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorFront(
    _: *const SherpaOnnxVoiceActivityDetector,
) -> *const SherpaOnnxSpeechSegment {
    let (start, n) = if SEGMENTS.load(Relaxed) == 2 {
        (10_000, 500)
    } else {
        (13_000, 700)
    };
    Box::into_raw(Box::new(SherpaOnnxSpeechSegment {
        start,
        n,
        samples: ptr::null_mut(),
    }))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroySpeechSegment(p: *const SherpaOnnxSpeechSegment) {
    drop(Box::from_raw(p.cast_mut()));
    eprintln!("drop segment");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorReset(
    _: *const SherpaOnnxVoiceActivityDetector,
) {
    WINDOWS.store(0, Relaxed);
    SEGMENTS.store(0, Relaxed);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxVoiceActivityDetectorFlush(
    _: *const SherpaOnnxVoiceActivityDetector,
) {
    if mode("energy-retry") {
        assert_eq!(WINDOWS.load(Relaxed), 4);
    } else if mode("fallback") {
        assert_eq!(WINDOWS.load(Relaxed), 1);
    } else {
        assert_eq!(WINDOWS.load(Relaxed), 40);
        SEGMENTS.store(2, Relaxed);
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxCreateOfflineSpeechDenoiser(
    config: *const SherpaOnnxOfflineSpeechDenoiserConfig,
) -> *const SherpaOnnxOfflineSpeechDenoiser {
    let config = &*config;
    assert_eq!(CStr::from_ptr(config.model.gtcrn.model), c"denoise");
    assert_eq!(CStr::from_ptr(config.model.provider), c"cpu");
    assert_eq!(config.model.num_threads, 2);
    if mode("denoiser-fails") {
        ptr::null()
    } else {
        handle()
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyOfflineSpeechDenoiser(
    p: *const SherpaOnnxOfflineSpeechDenoiser,
) {
    free_handle(p, "denoiser");
}

static PENDING_AUDIO: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxOfflineSpeechDenoiserRun(
    _: *const SherpaOnnxOfflineSpeechDenoiser,
    samples: *const f32,
    n: i32,
    sample_rate: i32,
) -> *const SherpaOnnxDenoisedAudio {
    assert_eq!(sample_rate, 16000);
    let clean: Box<[f32]> = slice::from_raw_parts(samples, n as usize)
        .iter()
        .map(|sample| sample / 2.0)
        .collect();
    PENDING_AUDIO.fetch_add(1, Relaxed);
    Box::into_raw(Box::new(SherpaOnnxDenoisedAudio {
        samples: Box::into_raw(clean).cast(),
        n,
        sample_rate: if mode("fallback") { 8000 } else { sample_rate },
    }))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn SherpaOnnxDestroyDenoisedAudio(p: *const SherpaOnnxDenoisedAudio) {
    let clean = Box::from_raw(p.cast_mut());
    let samples = ptr::slice_from_raw_parts_mut(clean.samples.cast_mut(), clean.n as usize);
    drop(Box::from_raw(samples));
    PENDING_AUDIO.fetch_sub(1, Relaxed);
    eprintln!("drop audio");
}
