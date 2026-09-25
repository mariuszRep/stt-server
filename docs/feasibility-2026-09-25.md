# Single-executable feasibility evidence — 2026-09-25

Build: Windows x64, Rust 1.98.1, Visual Studio Build Tools 2022, CMake 4.4.3,
Vulkan SDK 1.4.357.0. `transcribe-cpp`/`transcribe-cpp-sys` 0.2.3 are pinned by
`Cargo.lock`; `vulkan` is enabled and `shared`/`dynamic-backends` are absent.
`GGML_NATIVE=OFF` was used for a portable x64 CPU baseline. A short Cargo target path
and `LIB` pointing to the Vulkan SDK were required on this machine.

Test model: Handy Parakeet Unified EN 0.6B Q8 at immutable revision
`7e948f21b7bdbac698d3318db9d350f1096f3b6c`. Local SHA-256:
`4B50B6DD862BF6E346929AAF4F5EAACEC003BFA3F56462D6C874B41EF2F38795`,
matching Handy's catalog. Audio: existing nested `stt-server` 16 kHz mono WAV fixture.

| Test | Observed result |
|---|---|
| Preferred backend on Intel Iris Xe | `observed_backend=Vulkan0`; produced transcript |
| Explicit `--cpu` | `observed_backend=CPU`; produced the same transcript |
| Invalid `VK_ICD_FILENAMES` and `VK_DRIVER_FILES` | `observed_backend=CPU`, `fallback_reason=Vulkan backend unavailable`; produced the same transcript |

Transcript: “Well, I don't wish to see it any more, observed Phoebe, turning away her eyes it is certainly very like the old portrait”.

Release executable size: 57,476,608 bytes. SHA-256:
`E9C6EC3D48C7808B8190A48D3FAAE5770D90C0144E83963AA4933E75169F03E5`.
`dumpbin /DEPENDENTS` reports Windows API libraries, `vulkan-1.dll`, and MSVC C/C++
runtime libraries; it reports no separate transcribe/ggml/backend inference DLL.
No DLL is staged alongside the executable. Fresh-machine MSVC runtime availability,
older CPU instruction compatibility, Vulkan fallback on other GPU/driver combinations,
and model-family breadth remain to be verified.

Static-CRT correction: a first `RUSTFLAGS=-C target-feature=+crt-static` attempt failed at
link because the CMake native build retained mixed CRT defaults. Setting
`TRANSCRIBE_CMAKE_ARGS='-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'`
and rebuilding `transcribe-cpp-sys` produced a 64,034,304-byte server executable with SHA-256
`D4C6E65DB22FAE1EA2611E9AACD0E7954E1ED96EBEA0D91E21C2106C94A2D3D9`.
`dumpbin /DEPENDENTS` shows only Windows system DLLs and `vulkan-1.dll`: no MSVC C++ runtime
or inference DLL. Its two unit tests passed. A local HTTP smoke test loaded the existing
verified Parakeet Q8 model on `Vulkan0` and reproduced the same fixture transcript.
Handy already uses `transcribe-cpp` 0.2.3: its x64 Windows configuration enables
`dynamic-backends` plus Vulkan to ship per-ISA backend DLLs, while its Windows ARM
configuration links the CPU backend statically. This server uses the same inference crate
with Vulkan and no `dynamic-backends`, then aligns Rust and CMake on the static CRT to
meet its stricter one-executable packaging requirement.
