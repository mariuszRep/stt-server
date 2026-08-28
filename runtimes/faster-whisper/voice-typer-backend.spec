# -*- mode: python ; coding: utf-8 -*-
import os
from pathlib import Path

from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs

# "cpu" (default) omits the nvidia cuBLAS/cuDNN/cuda_runtime DLLs for a much smaller build
# that always runs on CPU; "gpu" bundles them so CUDA works out of the box. This is what
# ships in the installer (always cpu); CI builds "gpu" separately as a downloadable release
# asset for the in-app "Install GPU acceleration" flow. See the "Optional GPU/CUDA installer
# component" plan for why this exists.
build_variant = os.environ.get("VOICE_TYPER_BUILD_VARIANT", "cpu")


def optional_dynamic_libs(package):
    try:
        return collect_dynamic_libs(package)
    except Exception:
        return []


binaries = []
binaries += optional_dynamic_libs('ctranslate2')
if build_variant == "gpu":
    binaries += optional_dynamic_libs('nvidia.cublas')
    binaries += optional_dynamic_libs('nvidia.cuda_runtime')
    binaries += optional_dynamic_libs('nvidia.cudnn')

a = Analysis(
    ['run_sidecar.py'],
    pathex=['.'],
    binaries=binaries,
    datas=collect_data_files('faster_whisper'),
    hiddenimports=[
        'uvicorn.logging',
        'uvicorn.loops',
        'uvicorn.loops.auto',
        'uvicorn.protocols',
        'uvicorn.protocols.http',
        'uvicorn.protocols.http.auto',
        'uvicorn.protocols.websockets',
        'uvicorn.protocols.websockets.auto',
        'uvicorn.lifespan',
        'uvicorn.lifespan.on',
        'faster_whisper',
        'app',
        'app.main',
        'app.config',
        'app.transcribe',
        'app.download',
    ],
    hookspath=[],
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
)
pyz = PYZ(a.pure)
exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name='voice-typer-backend',
    debug=False,
    strip=False,
    upx=False,
    console=True,
)

# Sentinel read by both faster_whisper.rs::install_local (to confirm a found
# packaged exe actually matches the variant it's being asked for) and
# config.py's own device-resolution gate (to know its own build variant with
# certainty, independent of any DLL probe).
Path("dist").mkdir(parents=True, exist_ok=True)
Path("dist/variant.txt").write_text(build_variant, encoding="utf-8")
