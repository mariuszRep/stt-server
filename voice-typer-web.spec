# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the standalone web binary.
# Bundles the FastAPI backend + the pre-built frontend static files.
# Run `npm run build:web` (which calls this spec) — do not invoke directly.
from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs


def optional_dynamic_libs(package):
    try:
        return collect_dynamic_libs(package)
    except Exception:
        return []


binaries = []
binaries += optional_dynamic_libs('ctranslate2')
binaries += optional_dynamic_libs('nvidia.cublas')
binaries += optional_dynamic_libs('nvidia.cuda_runtime')
binaries += optional_dynamic_libs('nvidia.cudnn')


a = Analysis(
    ['run_sidecar.py'],
    pathex=['.'],
    binaries=binaries,
    datas=[('static', 'static')] + collect_data_files('faster_whisper'),
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
    name='voice-typer-web',
    debug=False,
    strip=False,
    upx=True,
    console=True,
)
