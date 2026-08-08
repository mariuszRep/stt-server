# Bootstrap installer for stt-server (Windows). Downloads the CPU-only binary only —
# GPU acceleration (if an NVIDIA GPU is present) is pulled separately, on demand, by
# `install` itself (see app/cli.py's _maybe_swap_to_gpu). Usage:
#
#   irm https://raw.githubusercontent.com/mariuszRep/stt-server/main/install.ps1 | iex
#
# Set $env:STT_SERVER_VERSION to pin a specific release (e.g. "0.2.0"); otherwise this
# always installs the latest published release.

$ErrorActionPreference = 'Stop'

$version = $env:STT_SERVER_VERSION
if ($version) {
    $url = "https://github.com/mariuszRep/stt-server/releases/download/v$version/stt-server-windows-cpu.exe"
} else {
    $url = "https://github.com/mariuszRep/stt-server/releases/latest/download/stt-server-windows-cpu.exe"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "stt-server-bootstrap.exe"
Write-Host "Downloading stt-server from $url ..."
Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing

Write-Host "Installing..."
& $tmp install
Remove-Item $tmp -Force -ErrorAction SilentlyContinue
