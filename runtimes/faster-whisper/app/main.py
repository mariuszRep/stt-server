import asyncio
import os
import platform as _platform
import secrets
import sys
import tempfile
import time
import wave
from pathlib import Path, PurePath

from fastapi import BackgroundTasks, Depends, FastAPI, File, Form, Header, HTTPException, UploadFile
from fastapi.concurrency import run_in_threadpool
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel

from app import config
from app.transcribe import _infer_lock, get_model, get_runtime_status, transcribe


async def require_auth(authorization: str | None = Header(default=None)) -> None:
    """Enforces config.AUTH_TOKEN (VOICE_TYPER_AUTH_TOKEN) as a bearer token on every route.

    Applied as a global dependency (see FastAPI(dependencies=[...]) below) rather than per-route,
    matching sherpad's require_auth middleware (runtimes/sherpa-onnx/crates/sherpad/src/api.rs) --
    no route is exempt, so a new route can't accidentally ship unauthenticated. Pure passthrough
    when no token is configured (the loopback-default case).
    """
    expected = config.AUTH_TOKEN
    if expected is None:
        return

    provided = None
    if authorization is not None and authorization.startswith("Bearer "):
        provided = authorization[len("Bearer "):]

    if provided is None or not secrets.compare_digest(provided, expected):
        raise HTTPException(status_code=401, detail="unauthorized")


app = FastAPI(title="Voice Typer Backend", version="0.1.0", dependencies=[Depends(require_auth)])

_ALLOWED_ORIGINS = [
    "http://localhost:5173",
    "http://127.0.0.1:5173",
    "http://localhost:4173",
    "http://127.0.0.1:4173",
    f"http://localhost:{config.PORT}",
    f"http://127.0.0.1:{config.PORT}",
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
]

app.add_middleware(
    CORSMiddleware,
    allow_origins=_ALLOWED_ORIGINS,
    allow_methods=["GET", "POST"],
    allow_headers=["*"],
)


class TranscriptWordResponse(BaseModel):
    word: str
    start: float
    end: float
    probability: float


class TranscriptSegmentResponse(BaseModel):
    text: str
    start: float
    end: float
    avg_logprob: float
    no_speech_prob: float
    compression_ratio: float
    words: list[TranscriptWordResponse] = []


class TranscriptionResponse(BaseModel):
    text: str
    # Additive: existing `{text}`-only consumers (the App, OpenDora) are unaffected.
    language: str | None = None
    duration: float | None = None
    segments: list[TranscriptSegmentResponse] = []


class HealthResponse(BaseModel):
    status: str
    model: str


class ConfigResponse(BaseModel):
    schema_version: int
    model: str
    host: str
    port: int
    device: str
    requested_device: str
    active_device: str
    device_source: str
    compute_type: str
    cuda_available: bool
    cuda_runtime_ok: bool
    cuda_supported_compute_types: list[str]
    cuda_error: str | None
    model_loaded: bool
    last_model_load_seconds: float | None
    last_decode_seconds: float | None = None
    last_queue_wait_seconds: float | None
    last_transcription_seconds: float | None
    last_total_seconds: float | None
    last_audio_duration_seconds: float | None = None
    last_rtf: float | None = None


class AdminRestartBody(BaseModel):
    model: str | None = None
    port: int | None = None
    host: str | None = None
    device: str | None = None
    compute_type: str | None = None
    auth_token: str | None = None


class AdminModelBody(BaseModel):
    model: str
    device: str | None = None
    compute_type: str | None = None


class AdminModelResponse(BaseModel):
    status: str
    model: str
    load_seconds: float | None = None


@app.get("/health")
async def health() -> HealthResponse:
    return HealthResponse(status="ok", model=config.MODEL)


def _create_warmup_wav() -> str:
    """Short silent WAV used only to prime the model/GPU at startup."""
    fd, path = tempfile.mkstemp(suffix=".wav")
    os.close(fd)
    sample_rate = 16000
    num_frames = int(sample_rate * 0.5)
    with wave.open(path, "wb") as wav_file:
        wav_file.setnchannels(1)
        wav_file.setsampwidth(2)
        wav_file.setframerate(sample_rate)
        wav_file.writeframes(b"\x00\x00" * num_frames)
    return path


async def _warm_up_model() -> None:
    """Loads the model and runs one throwaway inference at startup instead of on the user's
    first real chunk. Model load (~4-5s) and the GPU/cuDNN kernel-selection warm-up (another
    2-3s) are both one-time-per-process costs that `transcribe()` would otherwise pay lazily on
    whatever request happens to be first — see backend.log timings from real sessions, where the
    first chunk after a restart took 8-9s total vs 1-4s for every chunk after it.
    """
    path = _create_warmup_wav()
    try:
        await run_in_threadpool(transcribe, path, None)
        print("[voice-typer] model warm-up complete", flush=True)
    except Exception as exc:
        # Don't let a failed warm-up take the backend down — worst case, the first real
        # request just pays the cold-start cost as it did before this existed.
        print(f"[voice-typer] model warm-up failed (first real request will pay the cost): {exc}", flush=True)
    finally:
        Path(path).unlink(missing_ok=True)


@app.on_event("startup")
async def _on_startup() -> None:
    asyncio.create_task(_warm_up_model())


@app.get("/v1/config")
async def get_config() -> ConfigResponse:
    runtime = get_runtime_status()
    return ConfigResponse(
        schema_version=4,
        model=config.MODEL,
        host=config.HOST,
        port=config.PORT,
        device=str(runtime["active_device"]),
        requested_device=str(runtime["requested_device"]),
        active_device=str(runtime["active_device"]),
        device_source=str(runtime["device_source"]),
        compute_type=str(runtime["compute_type"]),
        cuda_available=bool(runtime["cuda_available"]),
        cuda_runtime_ok=bool(runtime["cuda_runtime_ok"]),
        cuda_supported_compute_types=list(runtime["cuda_supported_compute_types"]),
        cuda_error=runtime["cuda_error"] if runtime["cuda_error"] is None else str(runtime["cuda_error"]),
        model_loaded=bool(runtime["model_loaded"]),
        last_model_load_seconds=runtime["last_model_load_seconds"],
        last_decode_seconds=runtime["last_decode_seconds"],
        last_queue_wait_seconds=runtime["last_queue_wait_seconds"],
        last_transcription_seconds=runtime["last_transcription_seconds"],
        last_total_seconds=runtime["last_total_seconds"],
        last_audio_duration_seconds=runtime["last_audio_duration_seconds"],
        last_rtf=runtime["last_rtf"],
    )


@app.post("/v1/audio/transcriptions", response_model=TranscriptionResponse)
async def audio_transcriptions(
    file: UploadFile = File(...),
    prompt: str | None = Form(default=None),
) -> TranscriptionResponse:
    if not file.filename:
        raise HTTPException(status_code=400, detail="No file provided")

    request_id = f"stt-{int(time.time() * 1000)}"
    started = time.perf_counter()
    print(f"[voice-typer] {request_id} received filename={file.filename}", flush=True)

    suffix = Path(file.filename).suffix or ".webm"
    with tempfile.NamedTemporaryFile(delete=False, suffix=suffix) as tmp:
        tmp.write(await file.read())
        tmp_path = tmp.name

    try:
        # Offload the blocking, CPU/GPU-bound inference to a worker thread so the
        # event loop stays responsive (health checks, uploads) while a chunk is
        # being transcribed. Inference itself is serialized inside transcribe().
        result = await run_in_threadpool(transcribe, tmp_path, prompt)
    except Exception as exc:
        elapsed = time.perf_counter() - started
        print(f"[voice-typer] {request_id} failed after {elapsed:.2f}s: {exc}", flush=True)
        raise HTTPException(status_code=500, detail=f"Transcription failed: {exc}")
    finally:
        Path(tmp_path).unlink(missing_ok=True)

    elapsed = time.perf_counter() - started
    print(f"[voice-typer] {request_id} completed in {elapsed:.2f}s chars={len(result.text)}", flush=True)
    return TranscriptionResponse(
        text=result.text,
        language=result.language,
        duration=result.duration,
        segments=[
            TranscriptSegmentResponse(
                text=segment.text,
                start=segment.start,
                end=segment.end,
                avg_logprob=segment.avg_logprob,
                no_speech_prob=segment.no_speech_prob,
                compression_ratio=segment.compression_ratio,
                words=[
                    TranscriptWordResponse(
                        word=word.word, start=word.start, end=word.end, probability=word.probability
                    )
                    for word in segment.words
                ],
            )
            for segment in result.segments
        ],
    )


@app.post("/v1/admin/model")
async def admin_switch_model(body: AdminModelBody) -> AdminModelResponse:
    """Swap the loaded model in-process, without restarting the server.

    Reuses get_model()'s existing model/device/compute_type mismatch check
    (transcribe.py) by mutating config.MODEL (and DEVICE/COMPUTE_TYPE, if
    given) before calling it — the same call transcribe() itself makes, so
    no new loading logic is needed here, just making config.MODEL mutable
    at runtime. Runs under the same _infer_lock every inference call already
    holds while loading, so a swap can't race in-flight inference; requests
    made after the swap resolves see the new model automatically.
    """

    def _swap() -> float | None:
        # config.MODEL_DIR is an explicit per-model download_root, computed by
        # the Rust side as `<data-root>/models/faster-whisper/<model_id>`
        # (faster_whisper.rs::cached_model_dir) -- a flat, predictable layout,
        # deliberately not HuggingFace's own hashed cache scheme. It is set
        # once at process launch for the *old* model only, so a swap that
        # changes config.MODEL without also recomputing MODEL_DIR would load
        # (or, worse, re-download) the new model into the old model's
        # directory instead of its own -- verified to actually happen before
        # this fix was added. Re-derive the new directory by stripping the
        # old model id's path components off the tail of the old
        # MODEL_DIR and re-joining the new model id, mirroring
        # cached_model_dir's own `root.join("faster-whisper").join(model_id)`
        # construction exactly.
        if config.MODEL_DIR is not None:
            old_dir = PurePath(config.MODEL_DIR)
            old_model_parts = PurePath(config.MODEL).parts
            if old_dir.parts[-len(old_model_parts):] == old_model_parts:
                root = PurePath(*old_dir.parts[: -len(old_model_parts)])
                config.MODEL_DIR = str(root.joinpath(*PurePath(body.model).parts))

        config.MODEL = body.model
        if body.device is not None:
            config.DEVICE = body.device
        if body.compute_type is not None:
            config.COMPUTE_TYPE = body.compute_type
        with _infer_lock:
            get_model(config.DEVICE, config.COMPUTE_TYPE)
        return get_runtime_status()["last_model_load_seconds"]

    try:
        load_seconds = await run_in_threadpool(_swap)
    except Exception as exc:
        raise HTTPException(status_code=500, detail=f"Model switch failed: {exc}")

    return AdminModelResponse(status="ok", model=config.MODEL, load_seconds=load_seconds)


@app.post("/v1/admin/restart")
async def admin_restart(body: AdminRestartBody, background: BackgroundTasks):
    """Restart the server with optional config overrides (web mode — non-Tauri)."""

    async def _do_restart() -> None:
        await asyncio.sleep(0.4)
        env = {**os.environ}
        if body.model is not None:
            env["VOICE_TYPER_MODEL"] = body.model
        if body.port is not None:
            env["VOICE_TYPER_PORT"] = str(body.port)
        if body.host is not None:
            env["VOICE_TYPER_HOST"] = body.host
        if body.device is not None:
            env["VOICE_TYPER_DEVICE"] = body.device
        if body.compute_type is not None:
            env["VOICE_TYPER_COMPUTE_TYPE"] = body.compute_type
        if body.auth_token is not None:
            env["VOICE_TYPER_AUTH_TOKEN"] = body.auth_token
        # When running under uvicorn --reload, the parent reloader process holds
        # the listening socket. Respawning without dealing with it races the
        # reloader's own respawn → "Address already in use" (WinError 10048 on
        # Windows, EADDRINUSE elsewhere). Kill the reloader first, on every
        # platform, so it actually releases the socket before we replace ourselves.
        if "--reload" in sys.argv:
            import signal
            import subprocess
            if _platform.system() != "Windows":
                signal.signal(signal.SIGTERM, signal.SIG_IGN)  # survive parent shutdown
            try:
                os.kill(os.getppid(), signal.SIGTERM)
            except OSError:
                pass
            await asyncio.sleep(0.6)
            subprocess.Popen([sys.executable] + sys.argv, env=env)
            os._exit(0)
        elif _platform.system() == "Windows":
            import subprocess
            subprocess.Popen([sys.executable] + sys.argv, env=env)
            os._exit(0)
        else:
            os.execve(sys.executable, [sys.executable] + sys.argv, env)

    background.add_task(_do_restart)
    return {"status": "restarting"}


@app.post("/v1/admin/stop")
async def admin_stop(background: BackgroundTasks):
    """Shut down the server (web mode — non-Tauri)."""

    async def _do_stop() -> None:
        await asyncio.sleep(0.4)
        if "--reload" in sys.argv:
            import signal
            if _platform.system() != "Windows":
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
            try:
                os.kill(os.getppid(), signal.SIGTERM)
            except OSError:
                pass
        os._exit(0)

    background.add_task(_do_stop)
    return {"status": "stopping"}


# Static file serving for the standalone web binary.
# When bundled with PyInstaller the frontend dist is included as 'static/'.
# Can also be set via VOICE_TYPER_SERVE_STATIC=/path/to/dist for local testing.
def _find_static() -> Path | None:
    env_dir = os.environ.get("VOICE_TYPER_SERVE_STATIC")
    if env_dir:
        return Path(env_dir)
    if getattr(sys, "frozen", False):
        candidate = Path(getattr(sys, "_MEIPASS", "")) / "static"
        if candidate.exists():
            return candidate
    return None


_static_dir = _find_static()
if _static_dir is not None:
    from fastapi.staticfiles import StaticFiles
    # StaticFiles is mounted as its own ASGI sub-app, so it does not go through
    # FastAPI(dependencies=[...]) -- require_auth does not apply here. Acceptable: this only
    # serves the bundled frontend's static assets, not the authenticated API surface, mirroring
    # sherpad's own documented carve-out (CORS preflight) as the one exemption to its blanket auth.
    app.mount("/", StaticFiles(directory=str(_static_dir), html=True), name="static")
