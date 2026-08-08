"""stt-server CLI: install/start/stop/status/logs on top of `serve`.

`serve` (or no subcommand) is byte-identical to the server's original
zero-argument invocation — Tauri's sidecar spawn (whisper-vibes,
apps/desktop/src-tauri) passes no CLI args, so it is unaffected by
everything else in this module.
"""
import json
import os
import platform
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from app import config

APP_NAME = "stt-server"
GPU_ASSET_ENV = "STT_SERVER_GPU_ASSET_URL"
WINDOWS_TASK_NAME = "STTServer"


def _is_frozen() -> bool:
    return bool(getattr(sys, "frozen", False))


def install_dir() -> Path:
    if platform.system() == "Windows":
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    d = base / APP_NAME
    d.mkdir(parents=True, exist_ok=True)
    return d


def binary_name() -> str:
    return f"{APP_NAME}.exe" if platform.system() == "Windows" else APP_NAME


def installed_binary_path() -> Path:
    return install_dir() / binary_name()


def pid_file() -> Path:
    return install_dir() / f"{APP_NAME}.pid"


def log_file() -> Path:
    return install_dir() / f"{APP_NAME}.log"


def _health_url() -> str:
    host = config.HOST if config.HOST not in ("0.0.0.0", "::") else "127.0.0.1"
    return f"http://{host}:{config.PORT}/health"


def _admin_stop_url() -> str:
    host = config.HOST if config.HOST not in ("0.0.0.0", "::") else "127.0.0.1"
    return f"http://{host}:{config.PORT}/v1/admin/stop"


def _read_pid() -> int | None:
    try:
        return int(pid_file().read_text().strip())
    except Exception:
        return None


def _pid_alive(pid: int) -> bool:
    if platform.system() == "Windows":
        # Query the Win32 API directly rather than shelling out to `tasklist`
        # and parsing its text output — observed on a GitHub Actions Windows
        # runner to be both slow (multi-second per call) and occasionally
        # flaky (a live PID briefly misreported as absent).
        import ctypes

        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        handle = ctypes.windll.kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return False
        try:
            exit_code = ctypes.c_ulong()
            if not ctypes.windll.kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
                return False
            return exit_code.value == STILL_ACTIVE
        finally:
            ctypes.windll.kernel32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists, just not ours to signal


def _http_get(url: str, timeout: float = 2.0) -> dict | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception:
        return None


def _http_post_empty(url: str, timeout: float = 2.0) -> bool:
    try:
        req = urllib.request.Request(url, data=b"", method="POST")
        with urllib.request.urlopen(req, timeout=timeout):
            return True
    except Exception:
        return False


def _target_binary() -> Path:
    """The binary/script `install`/`start` should launch for `serve`."""
    if _is_frozen():
        installed = installed_binary_path()
        return installed if installed.exists() else Path(sys.executable)
    return Path(sys.executable)


def _serve_args(extra: list[str]) -> list[str]:
    target = _target_binary()
    if _is_frozen():
        return [str(target), "serve", *extra]
    # Dev mode: re-invoke the same interpreter against run_sidecar.py.
    script = Path(__file__).resolve().parent.parent / "run_sidecar.py"
    return [sys.executable, str(script), "serve", *extra]


def cmd_serve(args) -> int:
    """Run the server in the foreground. Writes a PID file; optionally
    redirects stdout/stderr to --log-file. This is the exact behavior
    Tauri's sidecar spawn relies on when invoked with no arguments."""
    if getattr(args, "log_file", None):
        log_path = Path(args.log_file)
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_fh = open(log_path, "a", buffering=1)
        sys.stdout = log_fh
        sys.stderr = log_fh

    pid_path = pid_file()
    pid_path.parent.mkdir(parents=True, exist_ok=True)
    pid_path.write_text(str(os.getpid()))

    try:
        _serve_forever()
    finally:
        try:
            if pid_path.exists() and pid_path.read_text().strip() == str(os.getpid()):
                pid_path.unlink()
        except Exception:
            pass
    return 0


def _serve_forever() -> None:
    """The original run_sidecar.py behavior: uvicorn.run with a bind-retry
    loop for the Windows post-restart socket-release race."""
    import uvicorn

    max_attempts = 20
    for attempt in range(1, max_attempts + 1):
        try:
            uvicorn.run("app.main:app", host=config.HOST, port=config.PORT)
            break
        except SystemExit:
            if attempt == max_attempts:
                raise
            time.sleep(0.25)


def cmd_start(args) -> int:
    pid = _read_pid()
    if pid and _pid_alive(pid):
        print(f"{APP_NAME} is already running (pid {pid}).")
        return 0

    log_path = log_file()
    cmd = _serve_args(["--log-file", str(log_path)])
    kwargs: dict = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL, "stdin": subprocess.DEVNULL}
    if platform.system() == "Windows":
        kwargs["creationflags"] = subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    subprocess.Popen(cmd, **kwargs)

    # Give the child a moment to write its PID file before returning.
    for _ in range(20):
        time.sleep(0.25)
        if _read_pid():
            break
    print(f"Starting {APP_NAME} (logs: {log_path}). Run `{APP_NAME} status` to check readiness.")
    return 0


def cmd_stop(args) -> int:
    pid = _read_pid()
    if not pid or not _pid_alive(pid):
        print(f"{APP_NAME} is not running.")
        return 0

    if _http_post_empty(_admin_stop_url()):
        for _ in range(20):
            time.sleep(0.25)
            if not _pid_alive(pid):
                print(f"{APP_NAME} stopped.")
                return 0

    # Fall back to a direct kill if the clean shutdown didn't take.
    if platform.system() == "Windows":
        subprocess.run(["taskkill", "/PID", str(pid), "/F"], capture_output=True)
    else:
        import signal
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    print(f"{APP_NAME} stopped (forced).")
    return 0


def cmd_status(args) -> int:
    # Try the HTTP health check first — it's fast (short timeout) and
    # authoritative for "is this actually serving requests", which is what
    # callers care about. Only fall back to PID-based diagnostics (slower,
    # and OS-dependent) if the health check itself doesn't succeed.
    pid = _read_pid()
    health = _http_get(_health_url())
    if health:
        pid_note = f" (pid {pid})" if pid else ""
        print(f"{APP_NAME}: running{pid_note}, healthy — model {health.get('model')}.")
        return 0

    if not pid:
        print(f"{APP_NAME}: not installed/started (no pid file at {pid_file()}).")
        return 1
    if not _pid_alive(pid):
        print(f"{APP_NAME}: not running (stale pid file for {pid}).")
        return 1
    print(f"{APP_NAME}: running (pid {pid}), not yet responding to /health (starting up?).")
    return 2


def cmd_logs(args) -> int:
    log_path = log_file()
    if not log_path.exists():
        print(f"No log file at {log_path} yet — has `{APP_NAME} start` been run?")
        return 1
    lines = log_path.read_text(errors="replace").splitlines()
    tail = lines[-args.lines:] if args.lines else lines
    print("\n".join(tail))
    return 0


def _install_windows(target: Path) -> int:
    log_path = log_file()
    run_cmd = f'"{target}" serve --log-file "{log_path}"'
    result = subprocess.run(
        [
            "schtasks", "/Create", "/TN", WINDOWS_TASK_NAME,
            "/TR", run_cmd,
            "/SC", "ONLOGON",
            "/RL", "LIMITED",
            "/F",
        ],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        # Not fatal — some environments (locked-down corporate policy, restricted
        # sandboxes/service accounts) deny Task Scheduler access even for a
        # per-user, non-admin (/RL LIMITED) task. The server can still be run
        # via `stt-server start`/`serve`; only auto-start-at-login is affected.
        print(f"Could not register auto-start (Scheduled Task): {result.stderr.strip()}")
        print(f"{APP_NAME} will still start now, but won't auto-start at login. "
              f"Run `{APP_NAME} install` again after resolving Task Scheduler permissions, "
              f"or start it manually with `{APP_NAME} start`.")
        return 0
    print(f"Registered Scheduled Task '{WINDOWS_TASK_NAME}' — {APP_NAME} will start automatically at login.")
    return 0


def _install_linux(target: Path) -> int:
    unit_dir = Path.home() / ".config" / "systemd" / "user"
    unit_dir.mkdir(parents=True, exist_ok=True)
    unit_path = unit_dir / f"{APP_NAME}.service"
    log_path = log_file()
    unit_path.write_text(
        "[Unit]\n"
        f"Description={APP_NAME}\n"
        "After=network.target\n\n"
        "[Service]\n"
        f'ExecStart="{target}" serve --log-file "{log_path}"\n'
        "Restart=on-failure\n\n"
        "[Install]\n"
        "WantedBy=default.target\n"
    )
    subprocess.run(["systemctl", "--user", "daemon-reload"], capture_output=True)
    result = subprocess.run(
        ["systemctl", "--user", "enable", f"{APP_NAME}.service"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        # Common in headless/CI environments: no systemd user D-Bus session
        # available. Not fatal — the unit file is still written, and
        # `stt-server start` works standalone without the registered service.
        print(f"Wrote {unit_path}, but could not enable it via systemctl --user: {result.stderr.strip()}")
        print("This is expected in headless/CI environments with no systemd user session. "
              f"On a real desktop login session, re-run `{APP_NAME} install`, or run "
              f"`systemctl --user enable {APP_NAME}.service` manually.")
        return 0
    print(f"Registered systemd user unit at {unit_path} — {APP_NAME} will start automatically at login.")
    return 0


def _current_version() -> str:
    from app import __version__ as v
    return v


def _gpu_asset_url(version: str) -> str | None:
    override = os.environ.get(GPU_ASSET_ENV)
    if override:
        return override
    if platform.system() == "Windows":
        return f"https://github.com/mariuszRep/stt-server/releases/download/v{version}/stt-server-windows-cuda-runtime.zip"
    return None  # No Linux GPU runtime bundle published yet — matches today's Windows-only GPU support in build.yml.


def _maybe_swap_to_gpu(target: Path) -> Path:
    """If a CUDA-capable GPU is present but the installed binary lacks a
    working CUDA runtime, download the cuBLAS/cuDNN/cuda_runtime DLL bundle
    and extract it flat into install_dir() (same directory as `target`) —
    config._cuda_dll_search_dirs() already searches the frozen binary's own
    directory, so the DLLs are auto-discovered on the next start with no
    further wiring needed. This is *not* a binary swap: `target` itself never
    changes, only the DLLs sitting next to it. Mirrors whisper-vibes' in-app
    `download_and_install_gpu_backend` (apps/desktop/src-tauri/src/lib.rs):
    same env-var override for local testing, same fallback-to-CPU-on-failure.
    Reuses config.CUDA_AVAILABLE/CUDA_RUNTIME_OK directly — the same signal
    already used by GET /v1/config, not a reimplementation.

    Downloads via `curl` and extracts via `tar` (Windows' bundled tar.exe is
    bsdtar/libarchive, which extracts .zip directly) rather than
    urllib/zipfile — both tools have shipped built into Windows since the
    2018 update, so this needs no new Python dependency and no custom
    HTTP/zip-handling code to maintain."""
    if not (config.CUDA_AVAILABLE and not config.CUDA_RUNTIME_OK):
        return target  # No GPU, or GPU already has a working runtime — nothing to do.

    url = _gpu_asset_url(_current_version())
    if not url:
        print(f"NVIDIA GPU detected, but no CUDA runtime bundle is published for {platform.system()} yet — staying on CPU.")
        return target

    print(f"NVIDIA GPU detected without a working CUDA runtime — downloading the runtime bundle from {url} ...")
    dest_dir = target.parent
    tmp_zip = dest_dir / f"{target.name}.cuda-runtime.zip"
    try:
        subprocess.run(["curl", "-fsSL", "-o", str(tmp_zip), url], check=True, timeout=120)
        subprocess.run(["tar", "-xf", str(tmp_zip), "-C", str(dest_dir)], check=True, timeout=60)
        print(f"CUDA runtime installed alongside {target}.")
    except Exception as exc:
        print(f"CUDA runtime download/extract failed ({exc}) — staying on CPU.")
    finally:
        tmp_zip.unlink(missing_ok=True)
    return target


def cmd_install(args) -> int:
    if _is_frozen():
        current = Path(sys.executable).resolve()
        target = installed_binary_path()
        if not target.exists() or target.resolve() != current:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(current, target)
            if platform.system() != "Windows":
                target.chmod(target.stat().st_mode | 0o111)
            print(f"Installed {APP_NAME} to {target}")
        else:
            print(f"Already installed at {target}")
        target = _maybe_swap_to_gpu(target)
    else:
        print("Running from source (not a packaged binary) — registering startup using the current "
              "Python interpreter; GPU-build auto-swap only applies to packaged installs.")

    system = platform.system()
    if system == "Windows":
        rc = _install_windows(_target_binary())
    elif system == "Linux":
        rc = _install_linux(_target_binary())
    else:
        print(f"Automatic startup registration is not supported on {system} yet. Use `{APP_NAME} start` manually.")
        rc = 0

    if rc == 0:
        cmd_start(args)
    return rc


def _unregister_startup() -> None:
    system = platform.system()
    if system == "Windows":
        subprocess.run(
            ["schtasks", "/Delete", "/TN", WINDOWS_TASK_NAME, "/F"],
            capture_output=True,
        )
    elif system == "Linux":
        subprocess.run(["systemctl", "--user", "disable", "--now", f"{APP_NAME}.service"], capture_output=True)
        subprocess.run(["systemctl", "--user", "daemon-reload"], capture_output=True)
        unit_path = Path.home() / ".config" / "systemd" / "user" / f"{APP_NAME}.service"
        try:
            unit_path.unlink()
        except FileNotFoundError:
            pass


def cmd_uninstall(args) -> int:
    pid = _read_pid()
    if pid and _pid_alive(pid):
        cmd_stop(args)

    _unregister_startup()

    d = install_dir()
    if not d.exists():
        print(f"{APP_NAME} is not installed (no directory at {d}).")
        return 0

    self_path = Path(sys.executable).resolve() if _is_frozen() else None
    running_from_install_dir = bool(self_path and self_path.parent == d.resolve())

    if platform.system() == "Windows" and running_from_install_dir:
        # Can't delete our own running .exe on Windows (file is locked while
        # in use). Schedule the cleanup via a short-lived detached helper that
        # waits for this process to exit, then removes the install directory.
        bat_path = Path(os.environ.get("TEMP", str(d.parent))) / f"{APP_NAME}-uninstall.bat"
        bat_path.write_text(
            "@echo off\r\n"
            "timeout /t 2 /nobreak >nul\r\n"
            f'rmdir /s /q "{d}"\r\n'
            f'del /f /q "{bat_path}"\r\n'
        )
        subprocess.Popen(
            ["cmd", "/c", str(bat_path)],
            creationflags=subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
        )
        print(f"{APP_NAME} stopped and auto-start unregistered. Finishing removal of {d} "
              f"in the background (can't delete a running executable on Windows) — done in a couple seconds.")
    else:
        shutil.rmtree(d, ignore_errors=True)
        print(f"{APP_NAME} uninstalled — removed {d}.")
    return 0
