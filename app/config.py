import os
import ctypes
import glob
import site
import sys
from pathlib import Path


CUDA_REQUIRED_DLLS = ("cublas64_12.dll",)


def _cuda_dll_search_dirs() -> list[Path]:
    candidates: list[Path] = []

    explicit_dir = os.environ.get("VOICE_TYPER_CUDA_DLL_DIR")
    if explicit_dir:
        candidates.append(Path(explicit_dir))

    if getattr(sys, "frozen", False):
        candidates.append(Path(getattr(sys, "_MEIPASS", "")))
        candidates.append(Path(sys.executable).resolve().parent)

    try:
        site_dirs = [Path(p) for p in site.getsitepackages()]
    except Exception:
        site_dirs = []
    for base in site_dirs:
        candidates.extend(
            [
                base / "ctranslate2",
                base / "nvidia" / "cublas" / "bin",
                base / "nvidia" / "cuda_runtime" / "bin",
                base / "nvidia" / "cudnn" / "bin",
            ]
        )

    for path_dir in os.environ.get("PATH", "").split(os.pathsep):
        if path_dir:
            candidates.append(Path(path_dir))

    if os.name == "nt":
        for toolkit_dir in glob.glob(r"C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.*\bin"):
            candidates.append(Path(toolkit_dir))

    unique: list[Path] = []
    seen: set[str] = set()
    for candidate in candidates:
        try:
            resolved = candidate.resolve()
        except Exception:
            resolved = candidate
        key = str(resolved).lower()
        if key not in seen and resolved.exists():
            unique.append(resolved)
            seen.add(key)
    return unique


def _prepare_cuda_dll_search_path() -> None:
    if os.name != "nt":
        return
    for dll_dir in _cuda_dll_search_dirs():
        try:
            os.add_dll_directory(str(dll_dir))
        except Exception:
            pass


def _cuda_available() -> bool:
    try:
        _prepare_cuda_dll_search_path()
        import ctranslate2

        return ctranslate2.get_cuda_device_count() > 0
    except Exception:
        return False


def _cuda_supported_compute_types() -> list[str]:
    try:
        _prepare_cuda_dll_search_path()
        import ctranslate2

        return sorted(ctranslate2.get_supported_compute_types("cuda"))
    except Exception:
        return []


def _cuda_runtime_status() -> tuple[bool, str | None]:
    _prepare_cuda_dll_search_path()
    if not _cuda_available():
        return False, "No CUDA device was detected by ctranslate2."
    if os.name != "nt":
        return True, None

    missing: list[str] = []
    for dll in CUDA_REQUIRED_DLLS:
        try:
            ctypes.WinDLL(dll)
        except Exception as exc:
            missing.append(f"{dll}: {exc}")

    if missing:
        searched = ", ".join(str(path) for path in _cuda_dll_search_dirs())
        return (
            False,
            "CUDA runtime is incomplete. "
            + "; ".join(missing)
            + ". Install the CUDA 12 runtime/cuBLAS, add its bin directory to PATH, "
            + "or set VOICE_TYPER_CUDA_DLL_DIR. Searched: "
            + searched,
        )
    return True, None


HOST = os.environ.get("VOICE_TYPER_HOST", "127.0.0.1")
PORT = int(os.environ.get("VOICE_TYPER_PORT", "8000"))
MODEL = os.environ.get("VOICE_TYPER_MODEL", "Systran/faster-whisper-small")
CUDA_AVAILABLE = _cuda_available()
CUDA_SUPPORTED_COMPUTE_TYPES = _cuda_supported_compute_types()
CUDA_RUNTIME_OK, CUDA_ERROR = _cuda_runtime_status()
_device_env = os.environ.get("VOICE_TYPER_DEVICE")
REQUESTED_DEVICE = _device_env or ("cuda" if CUDA_AVAILABLE else "cpu")
REQUESTED_DEVICE_SOURCE = "manual" if _device_env else "auto"
REQUESTED_COMPUTE_TYPE = os.environ.get("VOICE_TYPER_COMPUTE_TYPE") or (
    "float16" if REQUESTED_DEVICE == "cuda" else "int8"
)
if REQUESTED_DEVICE == "cuda" and not CUDA_RUNTIME_OK:
    DEVICE = "cpu"
    DEVICE_SOURCE = "fallback"
    COMPUTE_TYPE = "int8"
else:
    DEVICE = REQUESTED_DEVICE
    DEVICE_SOURCE = REQUESTED_DEVICE_SOURCE
    COMPUTE_TYPE = REQUESTED_COMPUTE_TYPE
DEFAULT_LANGUAGE = os.environ.get("VOICE_TYPER_LANGUAGE", None)
BEAM_SIZE = int(os.environ.get("VOICE_TYPER_BEAM_SIZE", "5"))
VAD_FILTER = os.environ.get("VOICE_TYPER_VAD_FILTER", "1") not in ("0", "false", "False", "")
AUTH_TOKEN = os.environ.get("VOICE_TYPER_AUTH_TOKEN") or None


# RAM thresholds are a starting-point heuristic, not tuned against real benchmarks — easy to
# adjust here in one place since callers (the `detect` CLI command, and potentially `install`
# later) only ever see the resulting model id, never these thresholds directly.
_RAM_MODEL_TIERS = (
    (4096, "Systran/faster-whisper-tiny"),
    (8192, "Systran/faster-whisper-base"),
    (16384, "Systran/faster-whisper-small"),
    (32768, "Systran/faster-whisper-medium"),
)


def recommend_model(total_ram_mb: int | None, cuda_available: bool, cuda_runtime_ok: bool) -> str:
    if cuda_available and cuda_runtime_ok:
        return "Systran/faster-whisper-large-v3"
    if total_ram_mb is None:
        return MODEL
    for threshold, model in _RAM_MODEL_TIERS:
        if total_ram_mb < threshold:
            return model
    return "Systran/faster-whisper-medium"


def mark_cuda_fallback(error: Exception | str) -> None:
    global DEVICE, DEVICE_SOURCE, COMPUTE_TYPE, CUDA_RUNTIME_OK, CUDA_ERROR

    CUDA_RUNTIME_OK = False
    CUDA_ERROR = str(error)
    DEVICE = "cpu"
    DEVICE_SOURCE = "fallback"
    COMPUTE_TYPE = "int8"
