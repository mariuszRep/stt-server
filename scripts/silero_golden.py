#!/usr/bin/env python
"""Write the golden per-frame speech probabilities that tests/vad_golden.json holds.

Runs the original silero_vad.onnx under onnxruntime (the reference the pure-Rust forward pass in
src/vad.rs must match within 1e-3) over a 16 kHz mono 16-bit WAV, the way the Silero wrapper
does: 512-sample frames, the last 64 samples of the previous frame prepended as context, LSTM
state carried across frames, the last frame zero-padded. Needs `pip install onnxruntime numpy`.

    python scripts/silero_golden.py <silero_vad.onnx> tests/fixtures/speech-16k.wav tests/vad_golden.json
"""
import json
import sys
import wave

import numpy as np
import onnxruntime as ort

FRAME = 512
CONTEXT = 64


# The fixture as recorded and quietly attenuated: the second case keeps probabilities away from
# the 0/1 rails so the comparison also exercises mid-range values.
GAINS = [1.0, 0.03]


def frame_probabilities(model_path, samples):
    session = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
    state = np.zeros((2, 1, 128), dtype=np.float32)
    context = np.zeros((1, CONTEXT), dtype=np.float32)
    rate = np.array(16000, dtype=np.int64)
    probabilities = []
    for start in range(0, len(samples), FRAME):
        frame = np.zeros((1, FRAME), dtype=np.float32)
        chunk = samples[start : start + FRAME]
        frame[0, : len(chunk)] = chunk
        window = np.concatenate([context, frame], axis=1)
        output, state = session.run(None, {"input": window, "state": state, "sr": rate})
        context = window[:, -CONTEXT:]
        probabilities.append(float(output[0, 0]))
    return probabilities


def main():
    model_path, wav_path, target = sys.argv[1:4]
    with wave.open(wav_path) as wav:
        assert wav.getframerate() == 16000 and wav.getnchannels() == 1 and wav.getsampwidth() == 2
        pcm = np.frombuffer(wav.readframes(wav.getnframes()), dtype="<i2")
    samples = pcm.astype(np.float32) / 32768.0
    golden = {
        "frame_samples": FRAME,
        "cases": [
            {"gain": gain, "probabilities": frame_probabilities(model_path, samples * np.float32(gain))}
            for gain in GAINS
        ],
    }
    with open(target, "w") as out:
        json.dump(golden, out)
    print(f"{target}: {len(golden['cases'])} cases, {len(golden['cases'][0]['probabilities'])} frames each")


if __name__ == "__main__":
    main()
