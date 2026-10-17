#!/usr/bin/env python
"""Export the Silero VAD v5 16 kHz weights to a raw little-endian f32 blob.

Run once, offline; the blob is committed as assets/silero_v5_16k.f32 and embedded by
src/vad.rs. Not part of the build. Needs `pip install onnx numpy`.

    python scripts/export_silero_weights.py <silero_vad.onnx> [assets/silero_v5_16k.f32]

The source model is the MIT-licensed silero_vad.onnx (snakers4/silero-vad; the copy shipped in
the voice_activity_detector crate works). It holds an 8 kHz and a 16 kHz branch behind an `If`
on the sample rate; only the 16 kHz branch is exported. Tensors are written in the order
src/vad.rs reads them. Prints the SHA-256 of the blob (recorded in THIRD_PARTY_NOTICES.md).
"""
import hashlib
import sys

import numpy as np
import onnx
from onnx import numpy_helper

# (name inside the 16 kHz branch, expected shape) in blob order.
TENSORS = [
    ("stft.forward_basis_buffer", (258, 1, 256)),
    ("encoder.0.reparam_conv.weight", (128, 129, 3)),
    ("encoder.0.reparam_conv.bias", (128,)),
    ("encoder.1.reparam_conv.weight", (64, 128, 3)),
    ("encoder.1.reparam_conv.bias", (64,)),
    ("encoder.2.reparam_conv.weight", (64, 64, 3)),
    ("encoder.2.reparam_conv.bias", (64,)),
    ("encoder.3.reparam_conv.weight", (128, 64, 3)),
    ("encoder.3.reparam_conv.bias", (128,)),
    ("decoder.rnn.weight_ih", (512, 128)),
    ("decoder.rnn.weight_hh", (512, 128)),
    ("decoder.rnn.bias_ih", (512,)),
    ("decoder.rnn.bias_hh", (512,)),
    ("decoder.decoder.2.weight", (1, 128, 1)),
    ("decoder.decoder.2.bias", (1,)),
]


def sixteen_khz_branch(model):
    """The `then` branch of the top-level `If` (taken when sr == 16000)."""
    graph = model.graph
    equal = next(n for n in graph.node if n.op_type == "Equal")
    constant = next(n for n in graph.node if n.output[0] == equal.input[1])
    assert numpy_helper.to_array(constant.attribute[0].t) == 16000, "unexpected If condition"
    branch_if = next(n for n in graph.node if n.op_type == "If")
    return next(a.g for a in branch_if.attribute if a.name == "then_branch")


def main():
    source = sys.argv[1]
    target = sys.argv[2] if len(sys.argv) > 2 else "assets/silero_v5_16k.f32"
    branch = sixteen_khz_branch(onnx.load(source))
    found = {}
    for node in branch.node:
        if node.op_type == "Constant":
            found[node.output[0].split("__Inline_0__")[-1]] = numpy_helper.to_array(node.attribute[0].t)
    blob = bytearray()
    for name, shape in TENSORS:
        tensor = found[name]
        assert tensor.shape == shape and tensor.dtype == np.float32, (name, tensor.shape)
        blob += tensor.astype("<f4").tobytes()
    with open(target, "wb") as out:
        out.write(blob)
    print(f"{target}: {len(blob)} bytes, {len(blob) // 4} floats")
    print(f"sha256 {hashlib.sha256(blob).hexdigest()}")


if __name__ == "__main__":
    main()
