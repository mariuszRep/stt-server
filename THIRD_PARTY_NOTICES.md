# Third-Party Notices

## Silero VAD (https://github.com/snakers4/silero-vad)

`assets/silero_v5_16k.f32` holds the 16 kHz weights of Silero VAD v5, exported once with
`scripts/export_silero_weights.py` from `silero_vad.onnx` (the copy shipped in the
`voice_activity_detector` 0.2.1 crate). `src/vad.rs` re-implements the model's forward pass and
the `get_speech_timestamps` segmentation rules of the same project in Rust. SHA-256 of the blob:
`b8df2e6e32753b7aa47ab59571b0d9d0b490a223f8dc9118bb388efeaec6f8e3`. Silero VAD is licensed under
the MIT License:

```
MIT License

Copyright (c) 2020-present Silero Team

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 3D-Speaker CAM++ (https://github.com/modelscope/3D-Speaker)

`assets/campplus_vox_16k.f16` holds the weights of the CAM++ speaker-embedding model trained on
VoxCeleb (`iic/speech_campplus_sv_en_voxceleb_16k` on ModelScope, whose model card names the
Apache License 2.0 and which 3D-Speaker publishes under the same licence), exported once with
`scripts/export_campplus_weights.py` (inference-mode batch normalisation folded in, stored as
`f16`). `src/speaker/` re-implements the model's forward pass and 3D-Speaker's feature
pipeline in Rust. SHA-256 of the blob:
`11122726706b2cfe164966cdafb93cc7727176e1db9d45952ef999d90b53e65b`. The model was trained on
VoxCeleb2; the weights are the licensed artefact. 3D-Speaker is licensed under the Apache License,
Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0):

```
Copyright 3D-Speaker contributors (Alibaba Group)

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

## Handy (https://github.com/cjpais/Handy)

Small pieces of `src/download.rs` (the stall-timeout constant, the
`Content-Range` start parser, and the resume/416/mirror-fallback logic
shape) are adapted from Handy's `src-tauri/src/managers/model/download.rs`
at commit `8f9cf53`. Each adapted item is marked in place with a comment
naming the source file and commit. Handy is licensed under the MIT License:

```
MIT License

Copyright (c) 2025 CJ Pais

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
