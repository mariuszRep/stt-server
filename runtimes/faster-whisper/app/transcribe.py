import threading
import time
import wave
from dataclasses import dataclass, asdict, field

import numpy as np
from faster_whisper import WhisperModel

from app import config

WHISPER_SAMPLE_RATE = 16000

_model: WhisperModel | None = None
_model_device: str | None = None
_model_compute_type: str | None = None
_model_name: str | None = None
# faster-whisper's transcribe() is CPU/GPU-bound and not safe to run concurrently
# on a single model instance. Serialize inference so parallel requests queue
# instead of corrupting shared state or thrashing the device.
_infer_lock = threading.Lock()
_state_lock = threading.Lock()


@dataclass
class RuntimeStats:
    requested_device: str = config.REQUESTED_DEVICE
    active_device: str = config.DEVICE
    device_source: str = config.DEVICE_SOURCE
    compute_type: str = config.COMPUTE_TYPE
    cuda_available: bool = config.CUDA_AVAILABLE
    cuda_runtime_ok: bool = config.CUDA_RUNTIME_OK
    cuda_supported_compute_types: list[str] | None = None
    cuda_error: str | None = config.CUDA_ERROR
    model_loaded: bool = False
    last_model_load_seconds: float | None = None
    last_decode_seconds: float | None = None
    last_queue_wait_seconds: float | None = None
    last_transcription_seconds: float | None = None
    last_total_seconds: float | None = None
    last_audio_duration_seconds: float | None = None
    last_rtf: float | None = None


_runtime_stats = RuntimeStats()


def _set_stats(**updates: object) -> None:
    with _state_lock:
        for key, value in updates.items():
            setattr(_runtime_stats, key, value)


def get_runtime_status() -> dict[str, object]:
    with _state_lock:
        data = asdict(_runtime_stats)
    data.update(
        {
            "requested_device": config.REQUESTED_DEVICE,
            "active_device": config.DEVICE,
            "device_source": config.DEVICE_SOURCE,
            "compute_type": config.COMPUTE_TYPE,
            "cuda_available": config.CUDA_AVAILABLE,
            "cuda_runtime_ok": config.CUDA_RUNTIME_OK,
            "cuda_supported_compute_types": config.CUDA_SUPPORTED_COMPUTE_TYPES,
            "cuda_error": config.CUDA_ERROR,
        }
    )
    return data


def get_model(device: str, compute_type: str) -> WhisperModel:
    global _model, _model_device, _model_compute_type, _model_name
    if (
        _model is None
        or _model_device != device
        or _model_compute_type != compute_type
        or _model_name != config.MODEL
    ):
        start = time.perf_counter()
        print(
            f"[voice-typer] loading model={config.MODEL} device={device} compute_type={compute_type}",
            flush=True,
        )
        _model = WhisperModel(
            config.MODEL,
            device=device,
            compute_type=compute_type,
            download_root=config.MODEL_DIR,
        )
        _model_device = device
        _model_compute_type = compute_type
        _model_name = config.MODEL
        load_seconds = time.perf_counter() - start
        _set_stats(model_loaded=True, last_model_load_seconds=load_seconds)
        print(
            f"[voice-typer] model loaded in {load_seconds:.2f}s on {device}/{compute_type}",
            flush=True,
        )
    return _model


@dataclass
class TranscriptWord:
    word: str
    start: float
    end: float
    probability: float


@dataclass
class TranscriptSegment:
    text: str
    start: float
    end: float
    avg_logprob: float
    no_speech_prob: float
    compression_ratio: float
    words: list[TranscriptWord] = field(default_factory=list)


@dataclass
class TranscriptionOutput:
    text: str
    language: str | None
    duration: float | None
    segments: list[TranscriptSegment] = field(default_factory=list)


def _decode_wav_pcm_to_float32(path: str) -> tuple[np.ndarray, int] | None:
    """Decode a canonical (uncompressed) PCM WAV file straight to a float32
    array, bypassing faster-whisper's internal ffmpeg/PyAV decode — the
    client (use-loop-recorder.ts's encodeViaWorker) always writes exactly this
    format for the primary recording path. Returns None for anything this
    simple reader can't handle (compressed WAV, or a non-WAV upload such as
    the MediaRecorder webm fallback), so the caller falls back to handing the
    file path to faster-whisper as before.
    """
    try:
        with wave.open(path, "rb") as wav_file:
            if wav_file.getsampwidth() != 2 or wav_file.getcomptype() != "NONE":
                return None
            channels = wav_file.getnchannels()
            sample_rate = wav_file.getframerate()
            raw = wav_file.readframes(wav_file.getnframes())
    except (wave.Error, EOFError, OSError):
        return None

    pcm = np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0
    if channels > 1:
        pcm = pcm.reshape(-1, channels).mean(axis=1)
    return pcm, sample_rate


def _resample_to_whisper_rate(pcm: np.ndarray, src_rate: int) -> np.ndarray:
    if src_rate == WHISPER_SAMPLE_RATE:
        return pcm
    ratio = WHISPER_SAMPLE_RATE / src_rate
    n_out = max(1, int(len(pcm) * ratio))
    indices = np.linspace(0, len(pcm) - 1, n_out)
    return np.interp(indices, np.arange(len(pcm)), pcm).astype(np.float32)


def _run(
    model: WhisperModel, audio: str | np.ndarray, initial_prompt: str | None
) -> TranscriptionOutput:
    # word_timestamps=True costs a small amount of extra compute (the model's own
    # cross-attention pattern) but is what makes per-word start/end/probability
    # available at all; avg_logprob/no_speech_prob/compression_ratio are already
    # computed by the library internally either way, just surfaced here instead of
    # discarded.
    segments_iter, info = model.transcribe(
        audio,
        language=config.DEFAULT_LANGUAGE,
        beam_size=config.BEAM_SIZE,
        vad_filter=config.VAD_FILTER,
        initial_prompt=initial_prompt,
        word_timestamps=True,
    )
    segments: list[TranscriptSegment] = []
    for segment in segments_iter:
        words = [
            TranscriptWord(word=w.word.strip(), start=w.start, end=w.end, probability=w.probability)
            for w in (segment.words or [])
        ]
        segments.append(
            TranscriptSegment(
                text=segment.text.strip(),
                start=segment.start,
                end=segment.end,
                avg_logprob=segment.avg_logprob,
                no_speech_prob=segment.no_speech_prob,
                compression_ratio=segment.compression_ratio,
                words=words,
            )
        )
    text = " ".join(s.text for s in segments if s.text).strip()
    return TranscriptionOutput(text=text, language=info.language, duration=info.duration, segments=segments)


def _fmt(seconds: float | None, digits: int = 3) -> str:
    return f"{seconds:.{digits}f}s" if seconds is not None else "n/a"


def transcribe(audio_path: str, prompt: str | None = None) -> TranscriptionOutput:
    initial_prompt = prompt.strip() if prompt and prompt.strip() else None
    started_at = time.perf_counter()
    print(
        f"[voice-typer] transcription queued file={audio_path} requested={config.REQUESTED_DEVICE} active={config.DEVICE}/{config.COMPUTE_TYPE}",
        flush=True,
    )

    # Decode outside the model lock: this is plain CPU/numpy work unrelated to
    # the shared model instance, so concurrent chunks can decode in parallel
    # instead of queuing behind each other for no reason. Only a canonical PCM
    # WAV (the primary recorder path) is handled here; anything else (e.g. the
    # MediaRecorder webm fallback) falls through to faster-whisper's own
    # ffmpeg/PyAV decode inside model.transcribe(), unchanged from before —
    # decode_seconds stays None for that path since it isn't separately timed.
    decode_start = time.perf_counter()
    decoded = _decode_wav_pcm_to_float32(audio_path)
    decode_seconds = time.perf_counter() - decode_start if decoded is not None else None
    audio: str | np.ndarray = audio_path
    if decoded is not None:
        pcm, source_rate = decoded
        audio = _resample_to_whisper_rate(pcm, source_rate)

    pre_lock_at = time.perf_counter()
    with _infer_lock:
        queue_wait = time.perf_counter() - pre_lock_at
        result: TranscriptionOutput | None = None
        try:
            model = get_model(config.DEVICE, config.COMPUTE_TYPE)
            infer_start = time.perf_counter()
            result = _run(model, audio, initial_prompt)
            return result
        except Exception as exc:
            # CUDA device-count detection can report a device whose runtime
            # (cuBLAS/cuDNN DLLs) isn't actually loadable - ctranslate2 can
            # discover this either at model construction (get_model, now
            # inside this try) or on first inference. Fall back to CPU
            # rather than leaving the backend permanently broken.
            if config.DEVICE == "cpu":
                raise
            print(f"[voice-typer] CUDA inference failed, falling back to CPU: {exc}", flush=True)
            config.mark_cuda_fallback(exc)
            _set_stats(
                active_device=config.DEVICE,
                device_source=config.DEVICE_SOURCE,
                compute_type=config.COMPUTE_TYPE,
                cuda_runtime_ok=config.CUDA_RUNTIME_OK,
                cuda_error=config.CUDA_ERROR,
            )
            model = get_model("cpu", "int8")
            infer_start = time.perf_counter()
            result = _run(model, audio, initial_prompt)
            return result
        finally:
            infer_seconds = time.perf_counter() - infer_start if "infer_start" in locals() else None
            total_seconds = time.perf_counter() - started_at
            audio_duration = result.duration if result is not None else None
            rtf = (infer_seconds / audio_duration) if infer_seconds is not None and audio_duration else None
            _set_stats(
                active_device=config.DEVICE,
                device_source=config.DEVICE_SOURCE,
                compute_type=config.COMPUTE_TYPE,
                cuda_runtime_ok=config.CUDA_RUNTIME_OK,
                cuda_error=config.CUDA_ERROR,
                last_decode_seconds=decode_seconds,
                last_queue_wait_seconds=queue_wait,
                last_transcription_seconds=infer_seconds,
                last_total_seconds=total_seconds,
                last_audio_duration_seconds=audio_duration,
                last_rtf=rtf,
            )
            if infer_seconds is not None:
                print(
                    f"[voice-typer] transcription finished decode={_fmt(decode_seconds)} "
                    f"queue_wait={queue_wait:.2f}s inference={infer_seconds:.2f}s total={total_seconds:.2f}s "
                    f"audio={_fmt(audio_duration, 2)} rtf={_fmt(rtf, 2)} device={config.DEVICE}/{config.COMPUTE_TYPE}",
                    flush=True,
                )
