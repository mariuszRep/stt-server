#!/usr/bin/env python
"""Export the CAM++ (VoxCeleb) speaker-embedding weights to a raw little-endian f16 blob.

Run once, offline; the blob is committed as assets/campplus_vox_16k.f16 and embedded by
src/speaker/. Not part of the build. Needs `pip install torch numpy`.

    python -I scripts/export_campplus_weights.py <campplus_voxceleb.bin> [assets/campplus_vox_16k.f16]

The source is the Apache-2.0 checkpoint of 3D-Speaker's `iic/speech_campplus_sv_en_voxceleb_16k`
(ModelScope). It is loaded with `torch.load(weights_only=True)` and only read as tensors; no code
from the download is run. Inference-mode batch normalisation is folded away: wherever a
convolution is followed directly by a BatchNorm, the BatchNorm is merged into that convolution's
weight and bias; the BatchNorms that precede a ReLU (after a concatenation) are written as a
per-channel scale and shift. Tensors are written in forward-pass order, the order src/speaker/campplus.rs
reads them. Prints the SHA-256 of the blob (recorded in THIRD_PARTY_NOTICES.md and pinned by a
test in src/speaker/).
"""
import hashlib
import sys

import numpy as np
import torch

EPS = 1e-5  # nn.BatchNorm default
BLOCKS = (12, 24, 16)  # dense layers per CAMDenseTDNNBlock


def main():
    source = sys.argv[1]
    target = sys.argv[2] if len(sys.argv) > 2 else "assets/campplus_vox_16k.f16"
    state = torch.load(source, map_location="cpu", weights_only=True)
    tensor = lambda name: state[name].double().numpy()
    out = []

    def push(array):
        half = np.ascontiguousarray(array, dtype="<f2")
        assert np.isfinite(half).all(), "a weight overflows f16"
        out.append(half.tobytes())

    def bn_terms(prefix):
        """Scale and shift of the inference-mode BatchNorm under `prefix` (affine or not)."""
        var, mean = tensor(prefix + ".running_var"), tensor(prefix + ".running_mean")
        gamma = tensor(prefix + ".weight") if prefix + ".weight" in state else np.ones_like(var)
        beta = tensor(prefix + ".bias") if prefix + ".bias" in state else np.zeros_like(var)
        scale = gamma / np.sqrt(var + EPS)
        return scale, beta - mean * scale

    def conv_bn(conv, bn):
        """`conv` (weight, optional bias) followed by the BatchNorm `bn`, folded into one."""
        weight = tensor(conv + ".weight")
        bias = tensor(conv + ".bias") if conv + ".bias" in state else np.zeros(weight.shape[0])
        scale, shift = bn_terms(bn)
        push(weight * scale.reshape(-1, *[1] * (weight.ndim - 1)))
        push(bias * scale + shift)

    def conv_bias(conv):
        push(tensor(conv + ".weight"))
        push(tensor(conv + ".bias"))

    # Front end: 2-D residual convolutions.
    conv_bn("head.conv1", "head.bn1")
    for layer in ("layer1", "layer2"):
        for index in (0, 1):
            block = f"head.{layer}.{index}"
            conv_bn(block + ".conv1", block + ".bn1")
            conv_bn(block + ".conv2", block + ".bn2")
            if block + ".shortcut.0.weight" in state:
                conv_bn(block + ".shortcut.0", block + ".shortcut.1")
    conv_bn("head.conv2", "head.bn2")

    # Time-delay backbone.
    conv_bn("xvector.tdnn.linear", "xvector.tdnn.nonlinear.batchnorm")
    for number, layers in enumerate(BLOCKS, start=1):
        for index in range(1, layers + 1):
            layer = f"xvector.block{number}.tdnnd{index}"
            scale, shift = bn_terms(layer + ".nonlinear1.batchnorm")
            push(scale)
            push(shift)
            conv_bn(layer + ".linear1", layer + ".nonlinear2.batchnorm")
            push(tensor(layer + ".cam_layer.linear_local.weight"))
            conv_bias(layer + ".cam_layer.linear1")
            conv_bias(layer + ".cam_layer.linear2")
        transit = f"xvector.transit{number}"
        scale, shift = bn_terms(transit + ".nonlinear.batchnorm")
        push(scale)
        push(shift)
        push(tensor(transit + ".linear.weight"))
    scale, shift = bn_terms("xvector.out_nonlinear.batchnorm")
    push(scale)
    push(shift)
    conv_bn("xvector.dense.linear", "xvector.dense.nonlinear.batchnorm")

    blob = b"".join(out)
    with open(target, "wb") as handle:
        handle.write(blob)
    dense = state["xvector.dense.linear.weight"].shape
    print(f"{target}: {len(blob)} bytes, {len(blob) // 2} half floats; embedding {dense[0]}-d from {dense[1]} inputs")
    print(f"sha256 {hashlib.sha256(blob).hexdigest()}")


if __name__ == "__main__":
    main()
