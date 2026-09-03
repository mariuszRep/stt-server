import asyncio
import hmac
import json
import os
import platform as _platform
import sys
import tempfile
import time
import wave
from pathlib import Path, PurePath

from fastapi import BackgroundTasks, FastAPI, File, Form, Header, HTTPException, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.concurrency import run_in_threadpool
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel

from app import config
from app.transcribe import _infer_lock, get_model, get_runtime_status, transcribe
from app.streaming import (
    StreamingSession,
    create_session,
    pcm_s16le_to_float32,
    WHISPER_SAMPLE_RATE,
)

app = FastAPI(title="Voice Typer Backend", version="0.1.0")

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


class TranscriptionResponse(BaseModel):
    text: str


class HealthResponse(BaseModel):
    status: str
    model: str


class StreamingCapability(BaseModel):
    enabled: bool
    endpoint: str
    protocolVersion: int
    encodings: list[str]
    sampleRates: list[int]
    resample: bool
    channels: list[int]


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
    last_queue_wait_seconds: float | None
    last_transcription_seconds: float | None
    last_total_seconds: float | None
    streaming: StreamingCapability | None = None


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
        last_queue_wait_seconds=runtime["last_queue_wait_seconds"],
        last_transcription_seconds=runtime["last_transcription_seconds"],
        last_total_seconds=runtime["last_total_seconds"],
        streaming=StreamingCapability(
            enabled=True,
            endpoint="/v1/audio/stream",
            protocolVersion=1,
            encodings=["pcm_s16le"],
            sampleRates=[16000, 44100, 48000],
            resample=True,
            channels=[1],
        ),
    )


@app.post("/v1/audio/transcriptions", response_model=TranscriptionResponse)
async def audio_transcriptions(
    file: UploadFile = File(...),
    prompt: str | None = Form(default=None),
    authorization: str | None = Header(default=None),
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
        text = await run_in_threadpool(transcribe, tmp_path, prompt)
    except Exception as exc:
        elapsed = time.perf_counter() - started
        print(f"[voice-typer] {request_id} failed after {elapsed:.2f}s: {exc}", flush=True)
        raise HTTPException(status_code=500, detail=f"Transcription failed: {exc}")
    finally:
        Path(tmp_path).unlink(missing_ok=True)

    elapsed = time.perf_counter() - started
    print(f"[voice-typer] {request_id} completed in {elapsed:.2f}s chars={len(text)}", flush=True)
    return TranscriptionResponse(text=text)


@app.post("/v1/admin/model")
async def admin_switch_model(body: AdminModelBody) -> AdminModelResponse:
    """Swap the loaded model in-process, without restarting the server.

    Reuses get_model()'s existing model/device/compute_type mismatch check
    (transcribe.py) by mutating config.MODEL (and DEVICE/COMPUTE_TYPE, if
    given) before calling it — the same call transcribe() itself makes, so
    no new loading logic is needed here, just making config.MODEL mutable
    at runtime. Runs under the same _infer_lock every batch/streaming
    inference call already holds while loading, so a swap can't race
    in-flight inference in either direction; requests made after the swap
    resolves see the new model automatically.

    An already-open streaming session (see StreamingSession in streaming.py)
    keeps using the model it started with — it captured its own reference at
    creation and nothing re-derives it mid-session. That's deliberate: a
    swap should not change models out from under a dictation already in
    progress. Only new requests/sessions after this call see the new model.
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


# ── WebSocket streaming endpoint ────────────────────────────────────────


@app.websocket("/v1/audio/stream")
async def audio_stream(ws: WebSocket):
    """Realtime streaming transcription via WebSocket.

    Protocol: see protocol.md
    Security: Origin allowlist enforced here (CORSMiddleware does not guard WS).
    """
    origin = ws.headers.get("origin", "")
    if origin not in _ALLOWED_ORIGINS:
        await ws.close(code=4001, reason="Origin not allowed")
        return

    await ws.accept()

    session: StreamingSession | None = None
    client_sample_rate: int = WHISPER_SAMPLE_RATE
    process_task: asyncio.Task | None = None

    async def _process_loop():
        """Background loop: run LocalAgreement2 inference periodically."""
        while True:
            await asyncio.sleep(0.5)
            if session is None:
                continue
            try:
                async for event in session.process():
                    await ws.send_text(json.dumps(event))
            except Exception as exc:
                try:
                    await ws.send_text(json.dumps({
                        "type": "error",
                        "code": "internal",
                        "message": str(exc),
                        "retryable": True,
                    }))
                except Exception:
                    pass

    try:
        while True:
            msg = await ws.receive()

            if msg["type"] == "websocket.disconnect":
                break

            if "text" in msg:
                data = json.loads(msg["text"])
                msg_type = data.get("type")

                if msg_type == "start":
                    # Validate protocol version
                    pv = data.get("protocolVersion", 1)
                    if pv != 1:
                        await ws.send_text(json.dumps({
                            "type": "error",
                            "code": "unsupported_protocol_version",
                            "message": f"Server supports protocolVersion 1, got {pv}",
                            "retryable": False,
                        }))
                        await ws.close(code=4003)
                        return

                    # Validate encoding
                    encoding = data.get("encoding", "pcm_s16le")
                    if encoding != "pcm_s16le":
                        await ws.send_text(json.dumps({
                            "type": "error",
                            "code": "unsupported_format",
                            "message": f"Encoding '{encoding}' not supported. Use pcm_s16le.",
                            "retryable": False,
                        }))
                        await ws.close(code=4003)
                        return

                    # Auth check for LAN mode: the server must have a configured
                    # token (VOICE_TYPER_AUTH_TOKEN) and the client must present
                    # the same one. An unconfigured token fails closed rather than
                    # falling back to presence-only checking.
                    is_lan = config.HOST not in ("127.0.0.1", "localhost", "::1")
                    if is_lan:
                        client_token = data.get("auth") or ""
                        if not config.AUTH_TOKEN or not client_token or not hmac.compare_digest(
                            client_token, config.AUTH_TOKEN
                        ):
                            await ws.send_text(json.dumps({
                                "type": "error",
                                "code": "unauthorized",
                                "message": "Auth token required in LAN mode",
                                "retryable": False,
                            }))
                            await ws.close(code=4001)
                            return

                    client_sample_rate = data.get("sampleRate", WHISPER_SAMPLE_RATE)
                    language = data.get("language")
                    prompt = data.get("prompt")

                    session = await create_session(language=language, prompt=prompt)

                    await ws.send_text(json.dumps({
                        "type": "ready",
                        "sessionId": session.session_id,
                        "provider": "faster-whisper",
                        "protocolVersion": 1,
                        "model": config.MODEL,
                        "language": language or "auto",
                        "sampleRate": WHISPER_SAMPLE_RATE,
                        "channels": 1,
                    }))

                    # Start background processing loop
                    process_task = asyncio.create_task(_process_loop())

                elif msg_type == "stop":
                    if process_task:
                        process_task.cancel()
                        try:
                            await process_task
                        except asyncio.CancelledError:
                            pass
                    if session:
                        final = session.flush_final()
                        if final:
                            await ws.send_text(json.dumps(final))
                    await ws.send_text(json.dumps({
                        "type": "closed",
                        "reason": "client_stop",
                    }))
                    await ws.close()
                    return

                elif msg_type == "abort":
                    if process_task:
                        process_task.cancel()
                    await ws.send_text(json.dumps({
                        "type": "closed",
                        "reason": "client_abort",
                    }))
                    await ws.close()
                    return

            elif "bytes" in msg and session is not None:
                pcm = pcm_s16le_to_float32(msg["bytes"])
                session.feed(pcm, client_sample_rate)

    except WebSocketDisconnect:
        pass
    except Exception as exc:
        try:
            await ws.send_text(json.dumps({
                "type": "error",
                "code": "internal",
                "message": str(exc),
                "retryable": False,
            }))
        except Exception:
            pass
    finally:
        if process_task and not process_task.done():
            process_task.cancel()
        if ws.client_state.name == "CONNECTED":
            try:
                await ws.close()
            except Exception:
                pass


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
    app.mount("/", StaticFiles(directory=str(_static_dir), html=True), name="static")
