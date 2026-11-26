use std::env;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;
use std::ptr::{self, NonNull};
use std::slice;

use anyhow::{Context, Result, ensure};

#[allow(dead_code, non_snake_case, unsafe_op_in_unsafe_fn)]
mod bindings;

use bindings::*;

const SAMPLE_RATE: i32 = 16_000;
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * 300;
const MAX_RESULT: usize = 1024 * 1024;
const VAD_WINDOW: usize = 512;
const PAD_BEFORE: i64 = 7_200;
const PAD_AFTER: i64 = 3_200;
const USAGE: u8 = 64;

struct Options {
    encoder: CString,
    decoder: CString,
    joiner: CString,
    ctc: Option<CString>,
    tokens: CString,
    vocabulary: CString,
    threads: i32,
    hotwords: Option<(CString, f32)>,
    vad: Option<CString>,
    denoiser: Option<CString>,
    warmup: bool,
    normalize_gain: bool,
    soft_speech_retry: bool,
    feature_dim: i32,
    provider: CString,
    blank_penalty: f32,
    max_active_paths: i32,
    vad_threshold: f32,
    vad_min_silence: f32,
    max_piece_seconds: usize,
}

impl Options {
    fn parse() -> Option<Self> {
        let mut args = env::args_os().skip(1);
        let model = args.next()?;
        let threads = CString::new(args.next()?.as_bytes()).ok()?;

        let mut hotwords = None;
        let mut ctc = false;
        let mut vad = None;
        let mut denoiser = None;
        let mut warmup = false;
        let mut normalize_gain = false;
        let mut soft_speech_retry = false;
        let mut feature_dim = 128;
        let mut provider = CString::new("cpu").ok()?;
        let mut blank_penalty = 0.0_f32;
        let mut max_active_paths = 4_i32;
        let mut vad_threshold = 0.3_f32;
        let mut vad_min_silence = 0.45_f32;
        let mut max_piece_seconds = 20_usize;
        while let Some(flag) = args.next() {
            match flag.as_bytes() {
                b"--hotwords" => hotwords = Some((args.next()?, args.next()?)),
                b"--ctc" => ctc = true,
                b"--vad" => vad = Some(CString::new(args.next()?.as_bytes()).ok()?),
                b"--denoise" => denoiser = Some(CString::new(args.next()?.as_bytes()).ok()?),
                b"--warmup" => warmup = true,
                b"--normalize-gain" => normalize_gain = true,
                b"--soft-speech-retry" => soft_speech_retry = true,
                b"--feature-dim" => feature_dim = args.next()?.to_str()?.parse().ok()?,
                b"--provider" => provider = CString::new(args.next()?.as_bytes()).ok()?,
                b"--blank-penalty" => blank_penalty = args.next()?.to_str()?.parse().ok()?,
                b"--max-active-paths" => max_active_paths = args.next()?.to_str()?.parse().ok()?,
                b"--vad-threshold" => vad_threshold = args.next()?.to_str()?.parse().ok()?,
                b"--vad-min-silence" => vad_min_silence = args.next()?.to_str()?.parse().ok()?,
                b"--max-piece-seconds" => {
                    max_piece_seconds = args.next()?.to_str()?.parse().ok()?
                }
                _ => return None,
            }
        }

        if ctc && (feature_dim != 80 || hotwords.is_some())
            || !matches!(feature_dim, 80 | 128)
            || !(0.0..=2.0).contains(&blank_penalty)
            || !(1..=16).contains(&max_active_paths)
            || !(0.1..=0.9).contains(&vad_threshold)
            || !(0.1..=2.0).contains(&vad_min_silence)
            || !(5..=60).contains(&max_piece_seconds)
        {
            return None;
        }
        if provider.as_bytes() != b"cpu" {
            let path = std::path::Path::new(provider.to_str().ok()?.strip_prefix("cpu:")?);
            let canonical = path.canonicalize().ok()?;
            let root = std::path::Path::new(&model).canonicalize().ok()?;
            if !canonical.starts_with(root) || !canonical.is_file() {
                return None;
            }
        }
        let mut end = ptr::null_mut();
        // Match the original CLI's C numeric grammar, including signed thread
        // counts and hexadecimal scores.
        let count = unsafe { libc::strtol(threads.as_ptr(), &mut end, 10) };
        if end
            != threads
                .as_ptr()
                .wrapping_add(threads.as_bytes().len())
                .cast_mut()
            || !(1..=64).contains(&count)
        {
            return None;
        }
        let hotwords = match hotwords {
            Some((file, score)) => {
                let score = CString::new(score.as_bytes()).ok()?;
                let value = unsafe { libc::strtof(score.as_ptr(), &mut end) };
                if end
                    != score
                        .as_ptr()
                        .wrapping_add(score.as_bytes().len())
                        .cast_mut()
                    || !(0.5..=3.0).contains(&value)
                {
                    return None;
                }
                Some((CString::new(file.as_bytes()).ok()?, value))
            }
            None => None,
        };

        let model_file = |name: &str| {
            let directory = model.as_bytes();
            if directory.len() + 1 + name.len() >= 4096 {
                return None;
            }
            CString::new([directory, b"/", name.as_bytes()].concat()).ok()
        };
        Some(Self {
            encoder: model_file("encoder.int8.onnx")?,
            decoder: model_file("decoder.int8.onnx")?,
            joiner: model_file("joiner.int8.onnx")?,
            ctc: if ctc {
                Some(model_file("model.int8.onnx")?)
            } else {
                None
            },
            tokens: model_file("tokens.txt")?,
            vocabulary: model_file("bpe.vocab")?,
            threads: count as i32,
            hotwords,
            vad,
            denoiser,
            warmup,
            normalize_gain,
            soft_speech_retry,
            feature_dim,
            provider,
            blank_penalty,
            max_active_paths,
            vad_threshold,
            vad_min_silence,
            max_piece_seconds,
        })
    }
}

fn read_count(input: &mut impl Read) -> io::Result<Option<usize>> {
    let mut header = [0; 4];
    loop {
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    input.read_exact(&mut header[1..])?;
    Ok(Some(u32::from_le_bytes(header) as usize))
}

fn read_samples(input: &mut impl Read, count: usize) -> Result<Vec<f32>> {
    let mut pcm = Vec::new();
    pcm.try_reserve_exact(count * 2)?;
    pcm.resize(count * 2, 0);
    input.read_exact(&mut pcm).context("read PCM payload")?;

    let mut samples = Vec::new();
    samples.try_reserve_exact(count)?;
    let (pairs, _) = pcm.as_chunks::<2>();
    samples.extend(
        pairs
            .iter()
            .map(|&pair| f32::from(i16::from_le_bytes(pair)) / 32_768.0),
    );
    Ok(samples)
}

fn send_result(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(&(bytes.len() as u32).to_le_bytes())?;
    output.write_all(bytes)?;
    output.flush()
}

struct Native<'api, T> {
    _api: &'api SherpaApi,
    ptr: NonNull<T>,
    destroy: unsafe extern "C" fn(*const T),
}

impl<'api, T> Native<'api, T> {
    fn new(
        api: &'api SherpaApi,
        ptr: *const T,
        destroy: unsafe extern "C" fn(*const T),
    ) -> Option<Self> {
        Some(Self {
            _api: api,
            ptr: NonNull::new(ptr.cast_mut())?,
            destroy,
        })
    }

    fn as_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }

    fn get(&self) -> &T {
        // Non-null owned handles contain an initialized object until destruction.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> Drop for Native<'_, T> {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.as_ptr()) };
    }
}

struct Decoder<'api> {
    api: &'api SherpaApi,
    vad: Option<Native<'api, SherpaOnnxVoiceActivityDetector>>,
    denoiser: Option<Native<'api, SherpaOnnxOfflineSpeechDenoiser>>,
    recognizer: Native<'api, SherpaOnnxOfflineRecognizer>,
    maximum: usize,
    preserve_pauses: bool,
    normalize_gain: bool,
    soft_speech_retry: bool,
}

impl<'api> Decoder<'api> {
    // Borrow the options for the decoder lifetime to keep all model paths alive.
    fn new(api: &'api SherpaApi, options: &'api Options) -> Result<Self> {
        let mut config = SherpaOnnxOfflineRecognizerConfig {
            feat_config: SherpaOnnxFeatureConfig {
                sample_rate: SAMPLE_RATE,
                feature_dim: options.feature_dim,
            },
            model_config: SherpaOnnxOfflineModelConfig {
                tokens: options.tokens.as_ptr(),
                num_threads: options.threads,
                provider: options.provider.as_ptr(),
                ..Default::default()
            },
            decoding_method: c"greedy_search".as_ptr(),
            blank_penalty: options.blank_penalty,
            ..Default::default()
        };
        if let Some(model) = &options.ctc {
            config.model_config.nemo_ctc.model = model.as_ptr();
        } else {
            config.model_config.transducer = SherpaOnnxOfflineTransducerModelConfig {
                encoder: options.encoder.as_ptr(),
                decoder: options.decoder.as_ptr(),
                joiner: options.joiner.as_ptr(),
            };
            config.model_config.model_type = c"nemo_transducer".as_ptr();
        }
        if let Some((file, score)) = &options.hotwords {
            config.decoding_method = c"modified_beam_search".as_ptr();
            config.max_active_paths = options.max_active_paths;
            config.hotwords_file = file.as_ptr();
            config.hotwords_score = *score;
            config.model_config.modeling_unit = c"bpe".as_ptr();
            config.model_config.bpe_vocab = options.vocabulary.as_ptr();
        }

        let recognizer = Native::new(
            api,
            unsafe { (api.SherpaOnnxCreateOfflineRecognizer)(&config) },
            api.SherpaOnnxDestroyOfflineRecognizer,
        )
        .context("initialize offline recognizer")?;
        let vad = if let Some(model) = &options.vad {
            // App requests are at most one piece, so VAD never enters long-speech mode or regrows.
            let piece = options.max_piece_seconds as f32;
            let config = SherpaOnnxVadModelConfig {
                silero_vad: SherpaOnnxSileroVadModelConfig {
                    model: model.as_ptr(),
                    threshold: options.vad_threshold,
                    min_silence_duration: options.vad_min_silence,
                    min_speech_duration: 0.06,
                    window_size: VAD_WINDOW as i32,
                    max_speech_duration: piece,
                },
                sample_rate: SAMPLE_RATE,
                num_threads: 1,
                provider: c"cpu".as_ptr(),
                ..Default::default()
            };
            Some(
                Native::new(
                    api,
                    unsafe { (api.SherpaOnnxCreateVoiceActivityDetector)(&config, piece + 5.0) },
                    api.SherpaOnnxDestroyVoiceActivityDetector,
                )
                .context("initialize voice activity detector")?,
            )
        } else {
            None
        };
        let denoiser = if let Some(model) = &options.denoiser {
            let config = SherpaOnnxOfflineSpeechDenoiserConfig {
                model: SherpaOnnxOfflineSpeechDenoiserModelConfig {
                    gtcrn: SherpaOnnxOfflineSpeechDenoiserGtcrnModelConfig {
                        model: model.as_ptr(),
                    },
                    num_threads: options.threads,
                    provider: c"cpu".as_ptr(),
                    ..Default::default()
                },
            };
            Some(
                Native::new(
                    api,
                    unsafe { (api.SherpaOnnxCreateOfflineSpeechDenoiser)(&config) },
                    api.SherpaOnnxDestroyOfflineSpeechDenoiser,
                )
                .context("initialize offline speech denoiser")?,
            )
        } else {
            None
        };

        Ok(Self {
            api,
            vad,
            denoiser,
            recognizer,
            maximum: options.max_piece_seconds * SAMPLE_RATE as usize,
            preserve_pauses: options.ctc.is_some(),
            normalize_gain: options.normalize_gain,
            soft_speech_retry: options.soft_speech_retry,
        })
    }

    fn process(&self, samples: Vec<f32>, output: &mut impl Write) -> Result<()> {
        let api = self.api;
        let clean = self.denoiser.as_ref().and_then(|denoiser| {
            Native::new(
                api,
                unsafe {
                    (api.SherpaOnnxOfflineSpeechDenoiserRun)(
                        denoiser.as_ptr(),
                        samples.as_ptr(),
                        samples.len() as i32,
                        SAMPLE_RATE,
                    )
                },
                api.SherpaOnnxDestroyDenoisedAudio,
            )
        });
        let mut audio = samples;
        if let Some(denoised) = clean.as_ref().and_then(|clean| {
            let audio = clean.get();
            if audio.n > 0 && audio.sample_rate == SAMPLE_RATE && !audio.samples.is_null() {
                // The pinned API owns n readable samples for the result's lifetime.
                Some(unsafe { slice::from_raw_parts(audio.samples, audio.n as usize) })
            } else {
                None
            }
        }) {
            // Reuse the input allocation instead of keeping both buffers.
            audio.clear();
            audio.extend_from_slice(denoised);
        }
        drop(clean);
        // Quiet-input measurements found VAD deletions, so the optional gain runs
        // before VAD, after denoising. Gain never exceeds +20 dB.
        if self.normalize_gain {
            normalize_gain(&mut audio);
        }
        let audible = self.soft_speech_retry && audible_speech(&audio);
        let (audio, time_map, fallback) = if let Some(vad) = &self.vad {
            let (speech, time_map) = speech_only(api, vad.as_ptr(), &audio, self.preserve_pauses)?;
            let seconds = audio.len() as f32 / SAMPLE_RATE as f32;
            let mut stderr = io::stderr().lock();
            if speech.is_empty() {
                let _ = writeln!(stderr, "vad heard no speech in {seconds:.2} s");
                if !audible {
                    return send_result(output, b"{\"text\":\"\"}").context("write empty result");
                }
                let _ = writeln!(
                    stderr,
                    "energy gate found audible speech; retrying once without VAD"
                );
                (audio, time_map, None)
            } else {
                let kept = speech.len() as f32 / SAMPLE_RATE as f32;
                let _ = writeln!(stderr, "vad kept {kept:.2} of {seconds:.2} s");
                // Decoding is deterministic, so only audio that VAD changed is worth a retry.
                let fallback = (audible && speech != audio).then_some(audio);
                (speech, time_map, fallback)
            }
        } else {
            (audio, Vec::new(), None)
        };
        let mut response = self.decode_audio(audio, &time_map)?;
        if let Some(fallback) = fallback {
            let value: serde_json::Value = serde_json::from_slice(&response)?;
            if value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.trim().is_empty())
            {
                response = self.decode_audio(fallback, &[])?;
            }
        }
        send_result(output, &response).context("write offline stream result")
    }

    fn decode_audio(&self, audio: Vec<f32>, time_map: &[SpeechRange]) -> Result<Vec<u8>> {
        let pieces = split_samples(&audio, self.maximum);
        if pieces.len() == 1 {
            let decoded = self.decode(audio)?;
            if time_map.is_empty() {
                return Ok(decoded);
            }
            let mut result: serde_json::Value = serde_json::from_slice(&decoded)?;
            remap_timestamps(&mut result, 0, time_map);
            return Ok(serde_json::to_vec(&result)?);
        }
        let mut text = String::new();
        let mut chunks = Vec::new();
        for piece in pieces {
            let processed_start = piece.start as f64 / SAMPLE_RATE as f64;
            let processed_end = piece.end as f64 / SAMPLE_RATE as f64;
            let offset = source_seconds(processed_start, time_map);
            let duration = source_seconds(processed_end, time_map) - offset;
            let piece_start = piece.start;
            let mut result: serde_json::Value =
                serde_json::from_slice(&self.decode(audio[piece].to_vec())?)?;
            remap_timestamps(&mut result, piece_start, time_map);
            if !time_map.is_empty()
                && let Some(times) = result
                    .get_mut("timestamps")
                    .and_then(serde_json::Value::as_array_mut)
            {
                for time in times {
                    if let Some(value) = time.as_f64() {
                        *time = serde_json::json!((value - offset).max(0.0));
                    }
                }
            }
            let part = result["text"]
                .as_str()
                .context("missing transcript")?
                .trim();
            if !text.is_empty() && !part.is_empty() {
                text.push(' ');
            }
            text.push_str(part);
            chunks.push(serde_json::json!({"audio_offset_seconds":offset,"audio_duration_seconds":duration,"result":result}));
        }
        let response = serde_json::to_vec(&serde_json::json!({"text":text,"chunks":chunks}))?;
        ensure!(
            response.len() <= MAX_RESULT,
            "result exceeds {MAX_RESULT} bytes"
        );
        Ok(response)
    }

    fn decode(&self, audio: Vec<f32>) -> Result<Vec<u8>> {
        let api = self.api;
        let stream = Native::new(
            api,
            unsafe { (api.SherpaOnnxCreateOfflineStream)(self.recognizer.as_ptr()) },
            api.SherpaOnnxDestroyOfflineStream,
        )
        .context("create offline stream")?;
        unsafe {
            (api.SherpaOnnxAcceptWaveformOffline)(
                stream.as_ptr(),
                SAMPLE_RATE,
                audio.as_ptr(),
                audio.len() as i32,
            );
        }
        // AcceptWaveformOffline copies its input into the stream.
        drop(audio);
        unsafe { (api.SherpaOnnxDecodeOfflineStream)(self.recognizer.as_ptr(), stream.as_ptr()) };
        let result = Native::new(
            api,
            unsafe { (api.SherpaOnnxGetOfflineStreamResult)(stream.as_ptr()) },
            api.SherpaOnnxDestroyOfflineRecognizerResult,
        )
        .context("get offline stream result")?;
        let json = result.get().json;
        ensure!(!json.is_null(), "missing result JSON");
        let length = unsafe { libc::strnlen(json, MAX_RESULT + 1) };
        ensure!(length <= MAX_RESULT, "result exceeds {MAX_RESULT} bytes");
        let bytes = unsafe { slice::from_raw_parts(json.cast::<u8>(), length) };
        Ok(bytes.to_vec())
    }
}

fn speech_only(
    api: &SherpaApi,
    vad: *const SherpaOnnxVoiceActivityDetector,
    samples: &[f32],
    preserve_pauses: bool,
) -> Result<(Vec<f32>, Vec<SpeechRange>)> {
    let mut speech = Vec::new();
    let mut time_map = Vec::new();
    speech
        .try_reserve_exact(samples.len())
        .context("reserve voice activity buffer")?;

    unsafe {
        (api.SherpaOnnxVoiceActivityDetectorReset)(vad);
        for window in samples.chunks(VAD_WINDOW) {
            (api.SherpaOnnxVoiceActivityDetectorAcceptWaveform)(
                vad,
                window.as_ptr(),
                window.len() as i32,
            );
        }
        (api.SherpaOnnxVoiceActivityDetectorFlush)(vad);
    }

    let mut covered = 0;
    while unsafe { (api.SherpaOnnxVoiceActivityDetectorEmpty)(vad) } == 0 {
        let segment = Native::new(
            api,
            unsafe { (api.SherpaOnnxVoiceActivityDetectorFront)(vad) },
            api.SherpaOnnxDestroySpeechSegment,
        )
        .context("get voice activity segment")?;
        let start = i64::from(segment.get().start);
        let end = (start + i64::from(segment.get().n) + PAD_AFTER).min(samples.len() as i64);
        // CTC recognition is sensitive to replacing interior pauses with
        // synthetic audio. Keep the original timing inside the bounded piece.
        let start = if preserve_pauses && covered > 0 {
            covered
        } else {
            (start - PAD_BEFORE).max(covered)
        };
        if end > start {
            if covered > 0 && start > covered {
                speech.resize(speech.len() + 1600, 0.0);
            }
            time_map.push(SpeechRange {
                processed_start: speech.len(),
                processed_end: speech.len() + (end - start) as usize,
                source_start: start as usize,
            });
            speech.extend_from_slice(&samples[start as usize..end as usize]);
            covered = end;
        }
        drop(segment);
        unsafe { (api.SherpaOnnxVoiceActivityDetectorPop)(vad) };
    }
    Ok((speech, time_map))
}

struct SpeechRange {
    processed_start: usize,
    processed_end: usize,
    source_start: usize,
}

fn source_seconds(processed: f64, map: &[SpeechRange]) -> f64 {
    let sample = (processed.max(0.0) * SAMPLE_RATE as f64).round() as usize;
    let range = map
        .iter()
        .find(|range| sample < range.processed_end)
        .or_else(|| map.last());
    range.map_or(processed, |range| {
        (range.source_start
            + sample
                .saturating_sub(range.processed_start)
                .min(range.processed_end - range.processed_start)) as f64
            / SAMPLE_RATE as f64
    })
}

fn source_end_seconds(processed: f64, map: &[SpeechRange]) -> f64 {
    let sample = (processed.max(0.0) * SAMPLE_RATE as f64).round() as usize;
    let range = map
        .iter()
        .rev()
        .find(|range| sample > range.processed_start)
        .or_else(|| map.first());
    range.map_or(processed, |range| {
        (range.source_start
            + sample
                .saturating_sub(range.processed_start)
                .min(range.processed_end - range.processed_start)) as f64
            / SAMPLE_RATE as f64
    })
}

fn remap_timestamps(result: &mut serde_json::Value, offset: usize, map: &[SpeechRange]) {
    if map.is_empty() {
        return;
    }
    let Some(times) = result
        .get("timestamps")
        .and_then(serde_json::Value::as_array)
        .cloned()
    else {
        return;
    };
    let original = times
        .iter()
        .filter_map(serde_json::Value::as_f64)
        .collect::<Vec<_>>();
    if original.len() != times.len() {
        return;
    }
    let offset = offset as f64 / SAMPLE_RATE as f64;
    let mut reliable = Vec::new();
    if let Some(durations) = result
        .get_mut("durations")
        .and_then(serde_json::Value::as_array_mut)
    {
        for (index, duration) in durations.iter_mut().enumerate() {
            if let (Some(start), Some(length)) = (original.get(index), duration.as_f64()) {
                let start_sample = ((start + offset) * SAMPLE_RATE as f64).round() as usize;
                let end_sample = ((start + length + offset) * SAMPLE_RATE as f64).round() as usize;
                // An interval crossing a VAD join contains removed silence, not word duration.
                reliable.push(
                    length.is_finite()
                        && length >= 0.0
                        && map.iter().any(|range| {
                            start_sample >= range.processed_start
                                && start_sample < range.processed_end
                                && end_sample >= start_sample
                                && end_sample <= range.processed_end
                        }),
                );
                *duration = serde_json::json!(
                    (source_end_seconds(start + length + offset, map)
                        - source_seconds(start + offset, map))
                    .max(0.0)
                );
            } else {
                reliable.push(false);
            }
        }
    }
    if !reliable.is_empty() {
        result["durations_reliable"] = serde_json::json!(reliable);
    }
    result["timestamps"] = serde_json::json!(
        original
            .into_iter()
            .map(|time| source_seconds(time + offset, map))
            .collect::<Vec<_>>()
    );
}

fn audible_speech(audio: &[f32]) -> bool {
    // Require sustained energy as well as OpenWhispr's RMS/peak thresholds.
    let mut run = 0;
    let mut longest = 0;
    let mut speech_samples = 0;
    for window in audio.chunks(SAMPLE_RATE as usize / 50) {
        let rms = (window
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / window.len().max(1) as f64)
            .sqrt();
        let peak = window.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
        if rms >= 0.003 && peak >= 0.02 {
            speech_samples += window.len();
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest >= 3 && speech_samples >= SAMPLE_RATE as usize / 10
}

fn normalize_gain(audio: &mut [f32]) {
    let peak = audio.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
    // Leave silence and extremely low-level noise untouched.
    if peak < 0.0001 {
        return;
    }
    let gain = (std::f32::consts::FRAC_1_SQRT_2 / peak).clamp(1.0, 10.0);
    for sample in audio {
        *sample *= gain;
    }
}

fn split_samples(mut audio: &[f32], maximum: usize) -> Vec<std::ops::Range<usize>> {
    let mut pieces = Vec::new();
    let mut offset = 0;
    while audio.len() > maximum {
        let window = 1600;
        let start = maximum.saturating_sub(4 * SAMPLE_RATE as usize);
        let cut = audio[start..maximum]
            .windows(window)
            .step_by(320)
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let energy =
                    |values: &[f32]| values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
                energy(a).total_cmp(&energy(b))
            })
            .map_or(maximum, |(offset, _)| start + offset * 320 + window / 2);
        let cut = cut.min(audio.len().saturating_sub(SAMPLE_RATE as usize));
        pieces.push(offset..offset + cut);
        audio = &audio[cut..];
        offset += cut;
    }
    if !audio.is_empty() {
        pieces.push(offset..offset + audio.len());
    }
    pieces
}

fn run(options: &Options, input: &mut impl Read, output: &mut impl Write) -> Result<ExitCode> {
    let binary = env::current_exe().context("locate recognizer executable")?;
    let library = binary
        .parent()
        .context("locate recognizer runtime")?
        .join("lib/libsherpa-onnx-c-api.so");
    let api = unsafe { SherpaApi::new(&library) }
        .with_context(|| format!("load {}", library.display()))?;
    let decoder = Decoder::new(&api, options)?;
    if options.warmup {
        decoder.decode(vec![0.0; SAMPLE_RATE as usize])?;
    }
    send_result(output, b"{\"ready\":true}").context("write readiness")?;

    loop {
        let count = match read_count(input) {
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Ok(Some(count)) if (1..=MAX_SAMPLES).contains(&count) => count,
            _ => return Ok(ExitCode::from(USAGE)),
        };
        decoder.process(read_samples(input, count)?, output)?;
        #[cfg(target_env = "gnu")]
        unsafe {
            libc::malloc_trim(0);
        }
    }
}

fn main() -> ExitCode {
    let Some(options) = Options::parse() else {
        return ExitCode::from(USAGE);
    };
    match run(&options, &mut io::stdin().lock(), &mut io::stdout().lock()) {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn energy_retry_rejects_silence_and_clicks() {
        assert!(!audible_speech(&vec![0.0; 16000]));
        assert!(!audible_speech(&vec![0.0001; 16000]));
        let mut click = vec![0.0; 16000];
        click[100] = 0.5;
        assert!(!audible_speech(&click));
        assert!(audible_speech(&vec![0.03; 1600]));
    }

    #[test]
    fn vad_time_mapping_restores_removed_silence_and_chunk_offsets() {
        let ranges = vec![
            SpeechRange {
                processed_start: 0,
                processed_end: 16000,
                source_start: 32000,
            },
            SpeechRange {
                processed_start: 17600,
                processed_end: 33600,
                source_start: 96000,
            },
        ];
        let mut result = serde_json::json!({"timestamps":[0.2,1.2],"durations":[0.2,0.3]});
        remap_timestamps(&mut result, 0, &ranges);
        assert_eq!(result["timestamps"], serde_json::json!([2.2, 6.1]));
        assert!((result["durations"][1].as_f64().unwrap() - 0.3).abs() < 0.0001);
        assert_eq!(source_seconds(1.05, &ranges), 6.0);
        let mut boundary = serde_json::json!({"timestamps":[0.5],"durations":[0.5]});
        remap_timestamps(&mut boundary, 0, &ranges);
        assert_eq!(boundary["durations"], serde_json::json!([0.5]));
        assert_eq!(boundary["durations_reliable"], serde_json::json!([true]));
        let mut crossing = serde_json::json!({"timestamps":[0.5],"durations":[0.7]});
        remap_timestamps(&mut crossing, 0, &ranges);
        assert_eq!(crossing["durations_reliable"], serde_json::json!([false]));
        assert_eq!(source_end_seconds(1.1, &ranges), 3.0);
    }

    #[test]
    fn direct_worker_splits_rebalance_short_tails() {
        for extra in [1600, 8000, 8001] {
            let audio = vec![0.5; 15 * 16000 + extra];
            let pieces = split_samples(&audio, 15 * 16000);
            assert!(
                pieces
                    .iter()
                    .all(|piece| piece.len() >= 16000 && piece.len() <= 15 * 16000)
            );
            assert_eq!(
                pieces.iter().map(|piece| piece.len()).sum::<usize>(),
                audio.len()
            );
        }
    }
    #[test]
    fn gain_preserves_silence_caps_boost_and_never_clips() {
        for input in [vec![0.0; 16], vec![0.00001; 16]] {
            let mut output = input.clone();
            normalize_gain(&mut output);
            assert_eq!(input, output);
        }
        let mut quiet = vec![0.001, -0.001];
        normalize_gain(&mut quiet);
        assert!((quiet[0] - 0.01).abs() < 1e-6);
        let mut normal = vec![0.5, -0.5];
        normalize_gain(&mut normal);
        assert!((normal[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        let mut loud = vec![1.0, -1.0];
        normalize_gain(&mut loud);
        assert_eq!(loud, [1.0, -1.0]);
    }
}
