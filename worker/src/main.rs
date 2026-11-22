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
    tokens: CString,
    vocabulary: CString,
    threads: i32,
    hotwords: Option<(CString, f32)>,
    vad: Option<CString>,
    denoiser: Option<CString>,
}

impl Options {
    fn parse() -> Option<Self> {
        let mut args = env::args_os().skip(1);
        let model = args.next()?;
        let threads = CString::new(args.next()?.as_bytes()).ok()?;

        let mut hotwords = None;
        let mut vad = None;
        let mut denoiser = None;
        while let Some(flag) = args.next() {
            match flag.as_bytes() {
                b"--hotwords" => hotwords = Some((args.next()?, args.next()?)),
                b"--vad" => vad = Some(CString::new(args.next()?.as_bytes()).ok()?),
                b"--denoise" => denoiser = Some(CString::new(args.next()?.as_bytes()).ok()?),
                _ => return None,
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
            tokens: model_file("tokens.txt")?,
            vocabulary: model_file("bpe.vocab")?,
            threads: count as i32,
            hotwords,
            vad,
            denoiser,
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
}

impl<'api> Decoder<'api> {
    // Borrow the options for the decoder lifetime to keep all model paths alive.
    fn new(api: &'api SherpaApi, options: &'api Options) -> Result<Self> {
        let mut config = SherpaOnnxOfflineRecognizerConfig {
            feat_config: SherpaOnnxFeatureConfig {
                sample_rate: SAMPLE_RATE,
                feature_dim: 128,
            },
            model_config: SherpaOnnxOfflineModelConfig {
                transducer: SherpaOnnxOfflineTransducerModelConfig {
                    encoder: options.encoder.as_ptr(),
                    decoder: options.decoder.as_ptr(),
                    joiner: options.joiner.as_ptr(),
                },
                tokens: options.tokens.as_ptr(),
                num_threads: options.threads,
                provider: c"cpu".as_ptr(),
                ..Default::default()
            },
            decoding_method: c"greedy_search".as_ptr(),
            ..Default::default()
        };
        if let Some((file, score)) = &options.hotwords {
            config.decoding_method = c"modified_beam_search".as_ptr();
            config.max_active_paths = 4;
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
            let config = SherpaOnnxVadModelConfig {
                silero_vad: SherpaOnnxSileroVadModelConfig {
                    model: model.as_ptr(),
                    threshold: 0.3,
                    min_silence_duration: 0.45,
                    min_speech_duration: 0.06,
                    window_size: VAD_WINDOW as i32,
                    max_speech_duration: 30.0,
                },
                sample_rate: SAMPLE_RATE,
                num_threads: 1,
                provider: c"cpu".as_ptr(),
                ..Default::default()
            };
            Some(
                Native::new(
                    api,
                    unsafe { (api.SherpaOnnxCreateVoiceActivityDetector)(&config, 300.0) },
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
        let audio = clean
            .as_ref()
            .and_then(|clean| {
                let audio = clean.get();
                if audio.n > 0 && audio.sample_rate == SAMPLE_RATE && !audio.samples.is_null() {
                    // The pinned API owns n readable samples for the result's lifetime.
                    Some(unsafe { slice::from_raw_parts(audio.samples, audio.n as usize) })
                } else {
                    None
                }
            })
            .unwrap_or(&samples);
        let speech = if let Some(vad) = &self.vad {
            let speech = speech_only(api, vad.as_ptr(), audio)?;
            let seconds = audio.len() as f32 / SAMPLE_RATE as f32;
            let mut stderr = io::stderr().lock();
            if speech.is_empty() {
                let _ = writeln!(stderr, "vad heard no speech; decoding all {seconds:.2} s");
            } else {
                let kept = speech.len() as f32 / SAMPLE_RATE as f32;
                let _ = writeln!(stderr, "vad kept {kept:.2} of {seconds:.2} s");
            }
            speech
        } else {
            Vec::new()
        };
        let audio = if speech.is_empty() { audio } else { &speech };

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
        drop(speech);
        drop(clean);
        drop(samples);

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
        send_result(output, bytes).context("write offline stream result")
    }
}

fn speech_only(
    api: &SherpaApi,
    vad: *const SherpaOnnxVoiceActivityDetector,
    samples: &[f32],
) -> Result<Vec<f32>> {
    let mut speech = Vec::new();
    if speech.try_reserve_exact(samples.len()).is_err() {
        return Ok(speech);
    }

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
        let start = (start - PAD_BEFORE).max(covered);
        if end > start {
            speech.extend_from_slice(&samples[start as usize..end as usize]);
            covered = end;
        }
        drop(segment);
        unsafe { (api.SherpaOnnxVoiceActivityDetectorPop)(vad) };
    }
    Ok(speech)
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
    send_result(output, b"{\"ready\":true}").context("write readiness")?;

    loop {
        let count = match read_count(input) {
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Ok(Some(count)) if (1..=MAX_SAMPLES).contains(&count) => count,
            _ => return Ok(ExitCode::from(USAGE)),
        };
        decoder.process(read_samples(input, count)?, output)?;
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
