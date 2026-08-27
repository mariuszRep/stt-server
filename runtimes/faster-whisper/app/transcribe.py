import threading
import time
from dataclasses import dataclass, asdict

from faster_whisper import WhisperModel

from app import config

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
    last_queue_wait_seconds: float | None = None
    last_transcription_seconds: float | None = None
    last_total_seconds: float | None = None


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
        _model = WhisperModel(config.MODEL, device=device, compute_type=compute_type)
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


def _run(model: WhisperModel, audio_path: str, initial_prompt: str | None) -> str:
    segments, _info = model.transcribe(
        audio_path,
        language=config.DEFAULT_LANGUAGE,
        beam_size=config.BEAM_SIZE,
        vad_filter=config.VAD_FILTER,
        initial_prompt=initial_prompt,
    )
    return " ".join(segment.text.strip() for segment in segments).strip()


def transcribe(audio_path: str, prompt: str | None = None) -> str:
    initial_prompt = prompt.strip() if prompt and prompt.strip() else None
    started_at = time.perf_counter()
    print(
        f"[voice-typer] transcription queued file={audio_path} requested={config.REQUESTED_DEVICE} active={config.DEVICE}/{config.COMPUTE_TYPE}",
        flush=True,
    )
    with _infer_lock:
        queue_wait = time.perf_counter() - started_at
        try:
            model = get_model(config.DEVICE, config.COMPUTE_TYPE)
            infer_start = time.perf_counter()
            return _run(model, audio_path, initial_prompt)
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
            return _run(model, audio_path, initial_prompt)
        finally:
            infer_seconds = time.perf_counter() - infer_start if "infer_start" in locals() else None
            total_seconds = time.perf_counter() - started_at
            _set_stats(
                active_device=config.DEVICE,
                device_source=config.DEVICE_SOURCE,
                compute_type=config.COMPUTE_TYPE,
                cuda_runtime_ok=config.CUDA_RUNTIME_OK,
                cuda_error=config.CUDA_ERROR,
                last_queue_wait_seconds=queue_wait,
                last_transcription_seconds=infer_seconds,
                last_total_seconds=total_seconds,
            )
            if infer_seconds is not None:
                print(
                    f"[voice-typer] transcription finished queue_wait={queue_wait:.2f}s inference={infer_seconds:.2f}s total={total_seconds:.2f}s device={config.DEVICE}/{config.COMPUTE_TYPE}",
                    flush=True,
                )
