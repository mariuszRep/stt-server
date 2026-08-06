"""Throughput spike test for the WS /v1/audio/stream endpoint.

Measures RTF (real-time factor) of faster-whisper-small int8 on CPU by
feeding synthetic 16kHz PCM over WebSocket and collecting partial/final
events with timestamps.

Usage:
    venv/bin/python tests/test_streaming_throughput.py

    # With a different model:
    VOICE_TYPER_MODEL=Systran/faster-whisper-tiny venv/bin/python tests/test_streaming_throughput.py
"""

import asyncio
import json
import time
import wave
import io
import os
import sys
import struct

import numpy as np

try:
    import websockets
except ImportError:
    print("Installing websockets...")
    os.system(f"{sys.executable} -m pip install websockets")
    import websockets

WS_URL = "ws://127.0.0.1:8000/v1/audio/stream"
SAMPLE_RATE = 16000
CHUNK_SEC = 0.5
CHUNK_SAMPLES = int(SAMPLE_RATE * CHUNK_SEC)
TOTAL_SEC = 10.0
TOTAL_CHUNKS = int(TOTAL_SEC / CHUNK_SEC)


def generate_tone(duration_sec: float, freq: float = 440.0, sr: int = 16000) -> bytes:
    """Generate a sine wave as PCM s16le bytes."""
    n = int(duration_sec * sr)
    t = np.linspace(0, duration_sec, n, endpoint=False)
    # Mix multiple frequencies to simulate speech-like spectral content
    wave_data = (
        0.3 * np.sin(2 * np.pi * freq * t)
        + 0.2 * np.sin(2 * np.pi * freq * 1.5 * t)
        + 0.1 * np.sin(2 * np.pi * freq * 2.0 * t)
        + 0.05 * np.random.randn(n)
    )
    # Apply amplitude envelope to simulate speech cadence
    envelope = np.ones(n)
    for i in range(0, n, sr // 4):
        env_start = i
        env_end = min(i + sr // 8, n)
        envelope[env_start:env_end] *= np.linspace(0.3, 1.0, env_end - env_start)
    wave_data *= envelope * 0.5
    pcm = (wave_data * 32767).astype("<i2")
    return pcm.tobytes()


async def run_test():
    print(f"Connecting to {WS_URL}...")
    print(f"Model: {os.environ.get('VOICE_TYPER_MODEL', 'Systran/faster-whisper-small')}")
    print(f"Sending {TOTAL_SEC}s of synthetic audio in {CHUNK_SEC}s chunks\n")

    events = []
    connect_start = time.perf_counter()

    async with websockets.connect(WS_URL, origin="http://127.0.0.1:5173") as ws:
        # Send start
        await ws.send(json.dumps({
            "type": "start",
            "protocolVersion": 1,
            "language": "en",
            "model": "auto",
            "encoding": "pcm_s16le",
            "sampleRate": SAMPLE_RATE,
            "channels": 1,
        }))

        # Wait for ready
        ready_msg = json.loads(await ws.recv())
        if ready_msg.get("type") != "ready":
            print(f"ERROR: expected ready, got {ready_msg}")
            return
        print(f"Ready: model={ready_msg['model']} session={ready_msg['sessionId'][:8]}...")
        print()

        # Feed audio chunks and collect events concurrently
        audio_start = time.perf_counter()

        async def send_audio():
            for i in range(TOTAL_CHUNKS):
                pcm = generate_tone(CHUNK_SEC, freq=200 + i * 50)
                await ws.send(pcm)
                await asyncio.sleep(CHUNK_SEC * 0.5)  # send faster than realtime
            # Send stop
            await ws.send(json.dumps({"type": "stop"}))

        async def receive_events():
            while True:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=30.0)
                except asyncio.TimeoutError:
                    print("TIMEOUT waiting for event")
                    break

                if isinstance(msg, bytes):
                    continue

                event = json.loads(msg)
                elapsed = time.perf_counter() - audio_start
                events.append((elapsed, event))

                if event["type"] == "partial":
                    print(f"  [{elapsed:6.2f}s] partial  id={event['id']:>8s}  text={event['text'][:60]!r}")
                elif event["type"] == "final":
                    print(f"  [{elapsed:6.2f}s] FINAL    id={event['id']:>8s}  text={event['text'][:60]!r}")
                elif event["type"] == "lagging":
                    print(f"  [{elapsed:6.2f}s] LAGGING  active={event['active']}")
                elif event["type"] == "error":
                    print(f"  [{elapsed:6.2f}s] ERROR    code={event['code']} msg={event['message']}")
                elif event["type"] == "closed":
                    print(f"  [{elapsed:6.2f}s] CLOSED   reason={event['reason']}")
                    break

        await asyncio.gather(send_audio(), receive_events())

    total_wall = time.perf_counter() - connect_start
    audio_wall = time.perf_counter() - audio_start

    # Compute stats
    partials = [e for _, e in events if e["type"] == "partial"]
    finals = [e for _, e in events if e["type"] == "final"]
    lagging = [e for _, e in events if e["type"] == "lagging"]
    errors = [e for _, e in events if e["type"] == "error"]

    print()
    print("=" * 60)
    print("THROUGHPUT SPIKE TEST RESULTS")
    print("=" * 60)
    print(f"  Audio duration:     {TOTAL_SEC:.1f}s")
    print(f"  Wall time (audio):  {audio_wall:.2f}s")
    print(f"  Wall time (total):  {total_wall:.2f}s")
    print(f"  RTF (wall/audio):   {audio_wall / TOTAL_SEC:.2f}x")
    print(f"  Partials received:  {len(partials)}")
    print(f"  Finals received:    {len(finals)}")
    print(f"  Lagging events:     {len(lagging)}")
    print(f"  Errors:             {len(errors)}")
    print()

    if finals:
        all_text = " ".join(e["text"] for _, e in events if e["type"] == "final")
        print(f"  Final transcript:   {all_text!r}")
    else:
        print("  No final transcripts received (expected for synthetic tone)")

    if lagging:
        print(f"  ⚠️  Backend reported lagging — RTF > 1.0 at some point")
    else:
        print(f"  ✅ No lagging events — backend kept up with audio")

    if errors:
        print(f"  ❌ {len(errors)} error events received")
    else:
        print(f"  ✅ No errors")

    print()
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(run_test()))
