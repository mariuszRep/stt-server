# Local dev gate: the same checks CI runs, with the static-build environment
# build-local.ps1 uses. Run by `npm run vt -- server dev`.
$ErrorActionPreference = 'Stop'
if (-not $env:VULKAN_SDK) { $env:VULKAN_SDK = 'C:\VulkanSDK\1.4.357.0' }
$env:PATH = "C:\Program Files\CMake\bin;$env:PATH"
$env:LIB = "$(Join-Path $env:VULKAN_SDK 'Lib');$env:LIB"
$env:TRANSCRIBE_CMAKE_ARGS = '-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo fmt --check
if ($LASTEXITCODE) { exit $LASTEXITCODE }
cargo clippy --release --all-targets -- -D warnings
if ($LASTEXITCODE) { exit $LASTEXITCODE }
cargo test --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }
