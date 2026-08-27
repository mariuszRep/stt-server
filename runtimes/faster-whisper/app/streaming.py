"""Streaming transcription adapter using LocalAgreement2 with faster-whisper."""

import asyncio
import json
import time
import uuid
from collections.abc import AsyncIterator

import numpy as np

from faster_whisper import WhisperModel

from app import config
from app.transcribe import get_model, _infer_lock

WHISPER_SAMPLE_RATE = 16000


def _resample(pcm: np.ndarray, src_rate: int, dst_rate: int) -> np.ndarray:
    if src_rate == dst_rate:
        return pcm
    ratio = dst_rate / src_rate
    n_out = int(len(pcm) * ratio)
    if n_out <= 1:
        return pcm[:1]
    indices = np.linspace(0, len(pcm) - 1, n_out)
    return np.interp(indices, np.arange(len(pcm)), pcm).astype(np.float32)


def pcm_s16le_to_float32(data: bytes) -> np.ndarray:
    arr = np.frombuffer(data, dtype="<i2").astype(np.float32)
    return arr / 32768.0


class StreamingSession:
    """LocalAgreement2 streaming session over a single WhisperModel."""

    def __init__(
        self,
        session_id: str,
        model: WhisperModel,
        language: str | None = None,
        prompt: str | None = None,
    ) -> None:
        self.session_id = session_id
        self._model = model
        self._language = language
        self._prompt = prompt
        self._buffer = np.array([], dtype=np.float32)
        self._committed_text = ""
        self._seg_counter = 0
        self._last_partial_id: str | None = None
        self._last_partial_text = ""
        self._committed_samples = 0
        self._lagging = False
        self._min_chunk_sec = 0.5
        # VISION.md: a configured ~30s chunk is a *soft* maximum — trimming only
        # happens once LocalAgreement2 has actually committed a prefix (a safe
        # boundary), so uninterrupted speech may run past this without being cut.
        self._soft_max_sec = 30.0
        # Explicit safety ceiling (CONVENTIONS.md allows one): if no safe boundary
        # has appeared and the buffer keeps growing past this, force a cut rather
        # than let re-transcription cost grow unbounded.
        self._hard_ceiling_sec = 60.0

    def feed(self, pcm_float32: np.ndarray, sample_rate: int) -> None:
        if sample_rate != WHISPER_SAMPLE_RATE:
            pcm_float32 = _resample(pcm_float32, sample_rate, WHISPER_SAMPLE_RATE)
        self._buffer = np.concatenate([self._buffer, pcm_float32])

    async def process(self) -> AsyncIterator[dict]:
        """Run one LocalAgreement2 iteration. Yields events as dicts."""
        min_samples = int(self._min_chunk_sec * WHISPER_SAMPLE_RATE)
        if len(self._buffer) < min_samples:
            return

        buffer_to_process = self._buffer.copy()

        t0 = time.perf_counter()
        try:
            segment_texts = await asyncio.to_thread(self._run_inference, buffer_to_process)
        except Exception as exc:
            yield {
                "type": "error",
                "code": "inference_failed",
                "message": str(exc),
                "retryable": True,
            }
            return

        infer_sec = time.perf_counter() - t0
        audio_sec = len(buffer_to_process) / WHISPER_SAMPLE_RATE
        rtf = infer_sec / audio_sec if audio_sec > 0 else 0

        was_lagging = self._lagging
        self._lagging = rtf > 1.0
        if was_lagging != self._lagging:
            yield {"type": "lagging", "active": self._lagging}

        full_text = " ".join(segment_texts).strip()

        # LocalAgreement2: find longest common prefix between consecutive outputs
        agreed, new_partial = self._find_agreement(self._last_partial_text, full_text)

        if agreed and len(agreed) > len(self._committed_text):
            final_text = agreed[len(self._committed_text):].strip()
            if final_text:
                self._seg_counter += 1
                seg_id = f"seg-{self._seg_counter}"
                self._committed_text = agreed
                # Estimate committed samples
                committed_frac = len(agreed) / max(len(full_text), 1)
                self._committed_samples = int(len(buffer_to_process) * committed_frac)
                yield {
                    "type": "final",
                    "id": seg_id,
                    "text": final_text,
                    "startMs": 0,
                    "endMs": int(audio_sec * 1000),
                }

        if new_partial and new_partial != self._last_partial_text:
            self._seg_counter += 1
            seg_id = f"seg-{self._seg_counter}"
            self._last_partial_id = seg_id
            self._last_partial_text = new_partial
            yield {
                "type": "partial",
                "id": seg_id,
                "text": new_partial,
                "startMs": 0,
                "endMs": int(audio_sec * 1000),
            }

        # Trim buffer at the soft-max boundary — only once something has been
        # committed, so this never cuts speech mid-utterance.
        trim_samples = int(self._soft_max_sec * WHISPER_SAMPLE_RATE)
        if self._committed_samples > 0 and len(self._buffer) > trim_samples:
            keep_from = self._committed_samples
            self._buffer = self._buffer[keep_from:]
            self._committed_samples = 0

        # Safety ceiling: nothing has been committed (e.g. continuous speech
        # LocalAgreement2 never agreed on a prefix for) and the buffer has grown
        # past the hard ceiling. Force everything uncommitted into a final and
        # reset, so buffer growth and re-transcription cost stay bounded.
        hard_ceiling_samples = int(self._hard_ceiling_sec * WHISPER_SAMPLE_RATE)
        if len(self._buffer) > hard_ceiling_samples:
            forced_text = full_text[len(self._committed_text):].strip()
            if forced_text:
                self._seg_counter += 1
                yield {
                    "type": "final",
                    "id": f"seg-{self._seg_counter}",
                    "text": forced_text,
                    "startMs": 0,
                    "endMs": int(audio_sec * 1000),
                }
            yield {
                "type": "error",
                "code": "buffer_overflow",
                "message": f"Audio buffer exceeded the {self._hard_ceiling_sec:.0f}s safety ceiling; forced a cut.",
                "retryable": True,
            }
            self._committed_text = ""
            self._last_partial_text = ""
            self._last_partial_id = None
            self._buffer = np.array([], dtype=np.float32)
            self._committed_samples = 0

    def _run_inference(self, audio: np.ndarray) -> list[str]:
        """Run Whisper inference in a thread (blocking, holds _infer_lock)."""
        with _infer_lock:
            segments_iter, _info = self._model.transcribe(
                audio,
                language=self._language,
                beam_size=1,
                # VAD is an optional, config-driven capability — not assumed on
                # or off — mirroring batch transcription's use of config.VAD_FILTER.
                vad_filter=config.VAD_FILTER,
                initial_prompt=self._prompt,
                without_timestamps=True,
            )
            return [s.text.strip() for s in segments_iter]

    def _find_agreement(self, prev: str, curr: str) -> tuple[str, str]:
        """Return (agreed_prefix, remaining_partial)."""
        prev_words = prev.split()
        curr_words = curr.split()
        agreed_count = 0
        for i in range(min(len(prev_words), len(curr_words))):
            if prev_words[i] == curr_words[i]:
                agreed_count = i + 1
            else:
                break
        agreed = " ".join(curr_words[:agreed_count]) if agreed_count > 0 else ""
        remaining = " ".join(curr_words[agreed_count:])
        return agreed, remaining

    def flush_final(self) -> dict | None:
        """Flush remaining partial as final on graceful stop."""
        if self._last_partial_text:
            remaining = self._last_partial_text[len(self._committed_text):].strip()
            if remaining:
                self._seg_counter += 1
                return {
                    "type": "final",
                    "id": f"seg-{self._seg_counter}",
                    "text": remaining,
                    "startMs": 0,
                    "endMs": int(len(self._buffer) / WHISPER_SAMPLE_RATE * 1000),
                }
        return None


async def create_session(
    language: str | None = None,
    prompt: str | None = None,
) -> StreamingSession:
    session_id = str(uuid.uuid4())
    try:
        model = get_model(config.DEVICE, config.COMPUTE_TYPE)
    except Exception as exc:
        if config.DEVICE == "cpu":
            raise
        print(
            f"[voice-typer] CUDA model load failed while starting a streaming session, falling back to CPU: {exc}",
            flush=True,
        )
        config.mark_cuda_fallback(exc)
        model = get_model("cpu", "int8")
    return StreamingSession(
        session_id=session_id,
        model=model,
        language=language or config.DEFAULT_LANGUAGE,
        prompt=prompt,
    )
