// Persistent sherpa-onnx recognizer. PCM and results travel only through pipes.
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "sherpa-onnx/c-api/c-api.h"

#define SAMPLE_RATE 16000
#define MAX_SAMPLES (SAMPLE_RATE * 300)
// Handy's VAD timing: 450 ms before speech, a short tail after it.
#define PAD_BEFORE (SAMPLE_RATE * 45 / 100)
#define PAD_AFTER (SAMPLE_RATE * 20 / 100)
#define VAD_WINDOW 512

static const SherpaOnnxVoiceActivityDetector *vad;
static const SherpaOnnxOfflineSpeechDenoiser *denoiser;

// Copy only the detected speech (with padding) into a new buffer. Returns the
// sample count, 0 when the detector heard no speech, or -1 on allocation failure.
static int32_t speech_only(const float *samples, int32_t count, float **output) {
  *output = malloc((size_t)count * sizeof(float));
  if (!*output)
    return -1;
  SherpaOnnxVoiceActivityDetectorReset(vad);
  // Silero consumes exactly one window per call, as in sherpa-onnx's examples;
  // a whole recording in one call is mostly ignored.
  for (int32_t offset = 0; offset < count; offset += VAD_WINDOW) {
    int32_t n = count - offset < VAD_WINDOW ? count - offset : VAD_WINDOW;
    SherpaOnnxVoiceActivityDetectorAcceptWaveform(vad, samples + offset, n);
  }
  SherpaOnnxVoiceActivityDetectorFlush(vad);
  int32_t written = 0, covered = 0;
  while (!SherpaOnnxVoiceActivityDetectorEmpty(vad)) {
    const SherpaOnnxSpeechSegment *segment =
        SherpaOnnxVoiceActivityDetectorFront(vad);
    int32_t start = segment->start - PAD_BEFORE;
    int32_t end = segment->start + segment->n + PAD_AFTER;
    if (start < covered)
      start = covered;
    if (end > count)
      end = count;
    if (end > start) {
      memcpy(*output + written, samples + start,
             (size_t)(end - start) * sizeof(float));
      written += end - start;
      covered = end;
    }
    SherpaOnnxDestroySpeechSegment(segment);
    SherpaOnnxVoiceActivityDetectorPop(vad);
  }
  return written;
}

static int read_count(uint32_t *value) {
  unsigned char bytes[4];
  size_t count = fread(bytes, 1, sizeof(bytes), stdin);
  if (count == 0 && feof(stdin))
    return 0;
  if (count != sizeof(bytes))
    return -1;
  *value = (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
  return 1;
}

static int send_result(const char *text) {
  size_t length = strlen(text);
  if (length > 1024 * 1024)
    return 0;
  unsigned char bytes[4] = {length & 255, (length >> 8) & 255,
                            (length >> 16) & 255, (length >> 24) & 255};
  return fwrite(bytes, 1, 4, stdout) == 4 &&
         fwrite(text, 1, length, stdout) == length && fflush(stdout) == 0;
}

// Usage: recognizer MODEL_DIR THREADS [--hotwords FILE SCORE] [--vad SILERO]
//        [--denoise GTCRN]
int main(int argc, char **argv) {
  if (argc < 3)
    return 64;
  const char *hotwords = NULL, *hotwords_score = NULL, *vad_model = NULL,
             *denoise_model = NULL;
  for (int i = 3; i < argc; i++) {
    if (!strcmp(argv[i], "--hotwords") && i + 2 < argc) {
      hotwords = argv[++i];
      hotwords_score = argv[++i];
    } else if (!strcmp(argv[i], "--vad") && i + 1 < argc) {
      vad_model = argv[++i];
    } else if (!strcmp(argv[i], "--denoise") && i + 1 < argc) {
      denoise_model = argv[++i];
    } else {
      return 64;
    }
  }
  char encoder[4096], decoder[4096], joiner[4096], tokens[4096],
      vocabulary[4096];
  if (snprintf(encoder, sizeof(encoder), "%s/encoder.int8.onnx", argv[1]) >=
          (int)sizeof(encoder) ||
      snprintf(decoder, sizeof(decoder), "%s/decoder.int8.onnx", argv[1]) >=
          (int)sizeof(decoder) ||
      snprintf(joiner, sizeof(joiner), "%s/joiner.int8.onnx", argv[1]) >=
          (int)sizeof(joiner) ||
      snprintf(tokens, sizeof(tokens), "%s/tokens.txt", argv[1]) >=
          (int)sizeof(tokens) ||
      snprintf(vocabulary, sizeof(vocabulary), "%s/bpe.vocab", argv[1]) >=
          (int)sizeof(vocabulary))
    return 64;
  char *end;
  long threads = strtol(argv[2], &end, 10);
  if (*end || threads < 1 || threads > 64)
    return 64;
  SherpaOnnxOfflineRecognizerConfig config = {0};
  config.feat_config.sample_rate = SAMPLE_RATE;
  config.feat_config.feature_dim = 128;
  config.model_config.transducer.encoder = encoder;
  config.model_config.transducer.decoder = decoder;
  config.model_config.transducer.joiner = joiner;
  config.model_config.tokens = tokens;
  config.model_config.num_threads = (int32_t)threads;
  config.model_config.provider = "cpu";
  config.decoding_method = "greedy_search";
  if (hotwords) {
    float score = strtof(hotwords_score, &end);
    if (*end || !isfinite(score) || score < 0.5f || score > 3.0f)
      return 64;
    config.decoding_method = "modified_beam_search";
    config.max_active_paths = 4;
    config.hotwords_file = hotwords;
    config.hotwords_score = score;
    config.model_config.modeling_unit = "bpe";
    config.model_config.bpe_vocab = vocabulary;
  }
  const SherpaOnnxOfflineRecognizer *recognizer =
      SherpaOnnxCreateOfflineRecognizer(&config);
  if (!recognizer)
    return 1;
  if (vad_model) {
    SherpaOnnxVadModelConfig vad_config = {0};
    vad_config.silero_vad.model = vad_model;
    vad_config.silero_vad.threshold = 0.3f;              // Handy: sensitive, keeps soft speech.
    vad_config.silero_vad.min_silence_duration = 0.45f;  // Handy's offline hangover.
    vad_config.silero_vad.min_speech_duration = 0.06f;   // Handy's onset.
    vad_config.silero_vad.window_size = VAD_WINDOW;
    vad_config.silero_vad.max_speech_duration = 30.0f;
    vad_config.sample_rate = SAMPLE_RATE;
    vad_config.num_threads = 1;
    vad_config.provider = "cpu";
    vad = SherpaOnnxCreateVoiceActivityDetector(&vad_config, 300.0f);
    if (!vad) {
      SherpaOnnxDestroyOfflineRecognizer(recognizer);
      return 1;
    }
  }
  if (denoise_model) {
    SherpaOnnxOfflineSpeechDenoiserConfig denoise_config = {0};
    denoise_config.model.gtcrn.model = denoise_model;
    denoise_config.model.num_threads = (int32_t)threads;
    denoise_config.model.provider = "cpu";
    denoiser = SherpaOnnxCreateOfflineSpeechDenoiser(&denoise_config);
    if (!denoiser) {
      SherpaOnnxDestroyOfflineRecognizer(recognizer);
      return 1;
    }
  }
  if (!send_result("{\"ready\":true}")) {
    SherpaOnnxDestroyOfflineRecognizer(recognizer);
    return 1;
  }
  int status = 0;
  uint32_t count;
  int input;
  while ((input = read_count(&count)) == 1) {
    if (!count || count > MAX_SAMPLES) {
      status = 64;
      break;
    }
    unsigned char *pcm = malloc((size_t)count * 2);
    float *samples = malloc((size_t)count * sizeof(float));
    if (!pcm || !samples || fread(pcm, 2, count, stdin) != count) {
      free(pcm);
      free(samples);
      status = 1;
      break;
    }
    for (uint32_t i = 0; i < count; i++) {
      int16_t sample =
          (int16_t)((uint16_t)pcm[2 * i] | ((uint16_t)pcm[2 * i + 1] << 8));
      samples[i] = sample / 32768.0f;
    }
    free(pcm);
    const float *audio = samples;
    int32_t length = (int32_t)count;
    const SherpaOnnxDenoisedAudio *clean = NULL;
    if (denoiser) {
      clean = SherpaOnnxOfflineSpeechDenoiserRun(denoiser, samples, length,
                                                 SAMPLE_RATE);
      if (clean && clean->n > 0 && clean->sample_rate == SAMPLE_RATE) {
        audio = clean->samples;
        length = clean->n;
      }
    }
    float *speech = NULL;
    if (vad) {
      int32_t kept = speech_only(audio, length, &speech);
      // No detected speech: decode everything rather than silently drop audio.
      if (kept > 0)
        fprintf(stderr, "vad kept %.2f of %.2f s\n", kept / (float)SAMPLE_RATE,
                length / (float)SAMPLE_RATE);
      else
        fprintf(stderr, "vad heard no speech; decoding all %.2f s\n",
                length / (float)SAMPLE_RATE);
      if (kept > 0) {
        audio = speech;
        length = kept;
      }
    }
    const SherpaOnnxOfflineStream *stream =
        SherpaOnnxCreateOfflineStream(recognizer);
    if (!stream) {
      free(speech);
      if (clean)
        SherpaOnnxDestroyDenoisedAudio(clean);
      free(samples);
      status = 1;
      break;
    }
    SherpaOnnxAcceptWaveformOffline(stream, SAMPLE_RATE, audio, length);
    free(speech);
    if (clean)
      SherpaOnnxDestroyDenoisedAudio(clean);
    free(samples);
    SherpaOnnxDecodeOfflineStream(recognizer, stream);
    const SherpaOnnxOfflineRecognizerResult *result =
        SherpaOnnxGetOfflineStreamResult(stream);
    int sent = result && result->json && send_result(result->json);
    if (result)
      SherpaOnnxDestroyOfflineRecognizerResult(result);
    SherpaOnnxDestroyOfflineStream(stream);
    if (!sent) {
      status = 1;
      break;
    }
  }
  if (input < 0)
    status = 64;
  if (vad)
    SherpaOnnxDestroyVoiceActivityDetector(vad);
  if (denoiser)
    SherpaOnnxDestroyOfflineSpeechDenoiser(denoiser);
  SherpaOnnxDestroyOfflineRecognizer(recognizer);
  return status;
}
