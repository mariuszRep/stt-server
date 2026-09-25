param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$repository = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $env:VULKAN_SDK) {
    $env:VULKAN_SDK = 'C:\VulkanSDK\1.4.357.0'
}
if (-not (Test-Path (Join-Path $env:VULKAN_SDK 'Lib\vulkan-1.lib'))) {
    throw "Vulkan SDK import library not found under $env:VULKAN_SDK"
}

$env:LIB = "$(Join-Path $env:VULKAN_SDK 'Lib');$env:LIB"
$env:TRANSCRIBE_CMAKE_ARGS = '-DGGML_NATIVE=OFF -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
$env:CARGO_TARGET_DIR = Join-Path $repository 's'
Remove-Item Env:LOCALAPPDATA -ErrorAction SilentlyContinue
Remove-Item Env:TEMP -ErrorAction SilentlyContinue

$arguments = @('build', '--release', '--bin', 'stt-server-next')
if ($Offline) { $arguments += '--offline' }
& cargo @arguments
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$binary = Join-Path $env:CARGO_TARGET_DIR 'release\stt-server-next.exe'
$file = Get-Item $binary
$checksum = (Get-FileHash $binary -Algorithm SHA256).Hash
Write-Output "Binary: $($file.FullName)"
Write-Output "Bytes: $($file.Length)"
Write-Output "SHA256: $checksum"
