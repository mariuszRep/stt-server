#!/usr/bin/env python
"""Write the golden features and embeddings that tests/speaker_golden.json holds.

Runs the original CAM++ (VoxCeleb) checkpoint under PyTorch (the reference the pure-Rust forward
pass in src/speaker/ must match: fbank within 1e-3, embeddings at cosine >= 0.9999) over a
16 kHz mono 16-bit WAV, the way 3D-Speaker's `infer_sv.py` does: torchaudio's Kaldi `fbank`
(80 mel bins, no dither) on the [-1, 1] waveform, per-recording mean subtraction, the network in
inference mode. The network is written out functionally over the checkpoint's tensors, so no code
from the download is imported. Needs `pip install torch torchaudio numpy`.

    python -I scripts/campplus_golden.py <campplus_voxceleb.bin> tests/fixtures/speech-16k.wav tests/speaker_golden.json
"""
import json
import sys
import wave

import numpy as np
import torch
import torch.nn.functional as F
import torchaudio.compliance.kaldi as kaldi

EPS = 1e-5
BLOCKS = ((12, 1), (24, 2), (16, 2))  # (dense layers, dilation) of the three CAM blocks
# The fixture as recorded and quietly attenuated.
GAINS = [1.0, 0.03]
# The golden file keeps this many feature frames (3 s) to stay small.
FBANK_FRAMES = 300


def fbank(samples):
    feat = kaldi.fbank(
        torch.from_numpy(samples).unsqueeze(0), num_mel_bins=80, sample_frequency=16000, dither=0.0
    )
    return feat - feat.mean(0, keepdim=True)


class Net:
    def __init__(self, state):
        self.state = state

    def batchnorm(self, x, prefix):
        s = self.state
        return F.batch_norm(
            x,
            s[prefix + ".running_mean"],
            s[prefix + ".running_var"],
            s.get(prefix + ".weight"),
            s.get(prefix + ".bias"),
            training=False,
            eps=EPS,
        )

    def conv(self, x, name, **kwargs):
        s = self.state
        op = F.conv2d if s[name + ".weight"].ndim == 4 else F.conv1d
        return op(x, s[name + ".weight"], s.get(name + ".bias"), **kwargs)

    def res_block(self, x, name, stride):
        out = F.relu(self.batchnorm(self.conv(x, name + ".conv1", stride=(stride, 1), padding=1), name + ".bn1"))
        out = self.batchnorm(self.conv(out, name + ".conv2", padding=1), name + ".bn2")
        if name + ".shortcut.0.weight" in self.state:
            x = self.batchnorm(self.conv(x, name + ".shortcut.0", stride=(stride, 1)), name + ".shortcut.1")
        return F.relu(out + x)

    def cam_layer(self, x, name, dilation):
        local = self.conv(x, name + ".linear_local", padding=dilation, dilation=dilation)
        segments = F.avg_pool1d(x, kernel_size=100, stride=100, ceil_mode=True)
        segments = segments.unsqueeze(-1).expand(*segments.shape, 100).reshape(*segments.shape[:-1], -1)
        context = x.mean(-1, keepdim=True) + segments[..., : x.shape[-1]]
        context = F.relu(self.conv(context, name + ".linear1"))
        return local * torch.sigmoid(self.conv(context, name + ".linear2"))

    def forward(self, features):
        """features: (frames, 80) -> the raw 512-d embedding."""
        x = features.t().unsqueeze(0).unsqueeze(0)  # (1, 1, 80, T)
        x = F.relu(self.batchnorm(self.conv(x, "head.conv1", padding=1), "head.bn1"))
        for layer in ("layer1", "layer2"):
            x = self.res_block(x, f"head.{layer}.0", 2)
            x = self.res_block(x, f"head.{layer}.1", 1)
        x = F.relu(self.batchnorm(self.conv(x, "head.conv2", stride=(2, 1), padding=1), "head.bn2"))
        x = x.reshape(1, x.shape[1] * x.shape[2], x.shape[3])
        x = F.relu(self.batchnorm(self.conv(x, "xvector.tdnn.linear", stride=2, padding=2), "xvector.tdnn.nonlinear.batchnorm"))
        for number, (layers, dilation) in enumerate(BLOCKS, start=1):
            for index in range(1, layers + 1):
                name = f"xvector.block{number}.tdnnd{index}"
                y = F.relu(self.batchnorm(x, name + ".nonlinear1.batchnorm"))
                y = F.relu(self.batchnorm(self.conv(y, name + ".linear1"), name + ".nonlinear2.batchnorm"))
                x = torch.cat([x, self.cam_layer(y, name + ".cam_layer", dilation)], dim=1)
            transit = f"xvector.transit{number}"
            x = self.conv(F.relu(self.batchnorm(x, transit + ".nonlinear.batchnorm")), transit + ".linear")
        x = F.relu(self.batchnorm(x, "xvector.out_nonlinear.batchnorm"))
        stats = torch.cat([x.mean(-1), x.std(-1, unbiased=True)], dim=-1)
        x = self.conv(stats.unsqueeze(-1), "xvector.dense.linear")
        return self.batchnorm(x, "xvector.dense.nonlinear.batchnorm").squeeze()


def main():
    checkpoint, wav_path, target = sys.argv[1:4]
    state = torch.load(checkpoint, map_location="cpu", weights_only=True)
    net = Net(state)
    with wave.open(wav_path) as wav:
        assert wav.getframerate() == 16000 and wav.getnchannels() == 1 and wav.getsampwidth() == 2
        pcm = np.frombuffer(wav.readframes(wav.getnframes()), dtype="<i2")
    samples = pcm.astype(np.float32) / 32768.0
    cases = []
    with torch.no_grad():
        for gain in GAINS:
            features = fbank(samples * np.float32(gain))
            embedding = F.normalize(net.forward(features), dim=0)
            case = {"gain": gain, "embedding": [round(float(v), 6) for v in embedding]}
            if gain == 1.0:
                case["fbank"] = [[round(float(v), 4) for v in row] for row in features[:FBANK_FRAMES]]
            cases.append(case)
    golden = {"dim": len(cases[0]["embedding"]), "fbank_frames": FBANK_FRAMES, "cases": cases}
    with open(target, "w") as out:
        json.dump(golden, out)
    print(f"{target}: {len(cases)} cases, {golden['dim']}-d")


if __name__ == "__main__":
    main()
