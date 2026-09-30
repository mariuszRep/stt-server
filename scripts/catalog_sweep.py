#!/usr/bin/env python3
"""Unattended catalog sweep for stt-server.

Starts an isolated server instance (own --data-dir and port, never the
user's real data/models), then walks every model+quant in the pinned
catalog one at a time: install -> verify (if needed) -> select -> transcribe
2 sample clips -> exercise each advertised capability once -> remove. Only
one model is ever on disk at a time. Designed to run unattended for hours
and be resumed (skips model/quant already present in --out unless --redo).

Python 3 stdlib only. See AGENTS.md / docs/client-contract.md for the
server's discovery, auth and API contract this script relies on.

Usage:
    python scripts/catalog_sweep.py --out sweep_results.jsonl \
        --clips-dir C:\\path\\to\\clips

    # small trial run against one model:
    python scripts/catalog_sweep.py --out trial.jsonl --clips-dir .\\clips \
        --only whisper-tiny --limit 1

    # build a clips dir from a bench_corpus.py results jsonl:
    python scripts/catalog_sweep.py --make-clips --from-results results.jsonl \
        --clips-dir C:\\path\\to\\clips
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import re
import shutil
import signal
import socket
import string
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

DEFAULT_EXE_DEBUG = r"D:\Users\mariu\Projects\voice-typer\stt-server\s\debug\stt-server.exe"
DEFAULT_EXE_RELEASE = r"D:\Users\mariu\Projects\voice-typer\stt-server\s\release\stt-server.exe"
DEFAULT_DATA_DIR = os.path.expandvars(
    r"%LOCALAPPDATA%\OpenVibeAI\STT Server Sweep"
)
DEFAULT_AUDIO_ROOT = os.path.expandvars(
    r"%APPDATA%\com.voicetyper.desktop\session-audio"
)
HEALTH_TIMEOUT_S = 30.0
DEFAULT_DOWNLOAD_TIMEOUT_S = 3600.0  # overridable with --download-timeout
INSTALL_TIMEOUT_S = DEFAULT_DOWNLOAD_TIMEOUT_S  # set from --download-timeout in main()
OPERATION_POLL_INTERVAL_S = 1.0
REQUEST_TIMEOUT_S = 180.0
CAPABILITY_TIMEOUT_S = 180.0

CAPABILITY_FIELD_ENDPOINT = {
    "prompt": "/v1/audio/transcriptions",
    "language_hint": "/v1/audio/transcriptions",
    "temperature": "/v1/audio/transcriptions",
    "word_timestamps": "/v1/audio/transcriptions",
    "translation": "/v1/audio/translations",
}


# ---------------------------------------------------------------------------
# Text normalisation + WER (mirrors scripts/bench_corpus.py's approach)
# ---------------------------------------------------------------------------

_CURLY_APOS = {"\u2018": "'", "\u2019": "'", "\u02bc": "'", "\u2032": "'"}


def normalize_text(text: str) -> str:
    if text is None:
        return ""
    s = text.lower()
    for curly, straight in _CURLY_APOS.items():
        s = s.replace(curly, straight)
    out = []
    n = len(s)
    for i, ch in enumerate(s):
        if ch.isalnum():
            out.append(ch)
        elif ch == "'":
            prev_ok = i > 0 and s[i - 1].isalnum()
            next_ok = i + 1 < n and s[i + 1].isalnum()
            out.append(ch if (prev_ok and next_ok) else " ")
        elif ch.isspace():
            out.append(" ")
        else:
            out.append(" ")
    return " ".join("".join(out).split())


def edit_distance(a: list, b: list) -> int:
    n, m = len(a), len(b)
    if n == 0:
        return m
    if m == 0:
        return n
    prev = list(range(m + 1))
    curr = [0] * (m + 1)
    for i in range(1, n + 1):
        curr[0] = i
        ai = a[i - 1]
        for j in range(1, m + 1):
            cost = 0 if ai == b[j - 1] else 1
            curr[j] = min(prev[j] + 1, curr[j - 1] + 1, prev[j - 1] + cost)
        prev, curr = curr, prev
    return prev[m]


def wer(reference: str, hypothesis: str):
    ref_norm = normalize_text(reference)
    hyp_norm = normalize_text(hypothesis)
    ref_words = ref_norm.split()
    hyp_words = hyp_norm.split()
    if not ref_words:
        return None
    dist = edit_distance(ref_words, hyp_words)
    return dist / len(ref_words)


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------

class ApiError(Exception):
    def __init__(self, status, body):
        self.status = status
        self.body = body
        super().__init__(f"HTTP {status}: {body}")


def http_json(method, url, token=None, payload=None, timeout=30.0):
    data = None
    headers = {}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = resp.read().decode("utf-8", errors="replace")
            return resp.status, (json.loads(body) if body else None)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        try:
            parsed = json.loads(body)
        except json.JSONDecodeError:
            parsed = {"raw": body}
        return e.code, parsed
    except urllib.error.URLError as e:
        raise ApiError(None, str(e.reason))


def build_multipart(fields: list, file_field_name: str, file_path: Path):
    boundary = uuid.uuid4().hex
    parts = []
    crlf = b"\r\n"
    for name, value in fields:
        parts.append(b"--" + boundary.encode() + crlf)
        parts.append(f'Content-Disposition: form-data; name="{name}"'.encode() + crlf)
        parts.append(crlf)
        parts.append(str(value).encode("utf-8") + crlf)
    with open(file_path, "rb") as f:
        file_data = f.read()
    parts.append(b"--" + boundary.encode() + crlf)
    parts.append(
        (
            f'Content-Disposition: form-data; name="{file_field_name}"; '
            f'filename="{file_path.name}"'
        ).encode()
        + crlf
    )
    parts.append(b"Content-Type: audio/wav" + crlf)
    parts.append(crlf)
    parts.append(file_data + crlf)
    parts.append(b"--" + boundary.encode() + b"--" + crlf)
    body = b"".join(parts)
    return body, f"multipart/form-data; boundary={boundary}"


def http_multipart(url, token, fields, audio_path, timeout):
    body, content_type = build_multipart(fields, "file", audio_path)
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", content_type)
    req.add_header("Content-Length", str(len(body)))
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status = resp.status
            raw = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        status = e.code
        raw = e.read().decode("utf-8", errors="replace")
    except urllib.error.URLError as e:
        return None, None, str(e.reason), (time.monotonic() - start) * 1000.0
    wall_ms = (time.monotonic() - start) * 1000.0
    try:
        parsed = json.loads(raw) if raw else None
    except json.JSONDecodeError:
        parsed = {"raw_text": raw}
    return status, parsed, None, wall_ms


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

def find_spare_port(start=54500, end=54900):
    for port in range(start, end):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.2)
            if s.connect_ex(("127.0.0.1", port)) != 0:
                return port
    raise RuntimeError("no spare port found")


class Server:
    def __init__(self, exe, data_dir, port, log_path):
        self.exe = exe
        self.data_dir = Path(data_dir)
        self.port = port
        self.base_url = f"http://127.0.0.1:{port}"
        self.log_path = log_path
        self.proc = None
        self.token = None

    def start(self):
        self.data_dir.mkdir(parents=True, exist_ok=True)
        log_f = open(self.log_path, "a", encoding="utf-8")
        creationflags = 0
        if os.name == "nt":
            creationflags = subprocess.CREATE_NEW_PROCESS_GROUP
        self.proc = subprocess.Popen(
            [
                self.exe,
                "run",
                "--port",
                str(self.port),
                "--host",
                "127.0.0.1",
                "--data-dir",
                str(self.data_dir),
            ],
            stdout=log_f,
            stderr=subprocess.STDOUT,
            creationflags=creationflags,
        )
        deadline = time.monotonic() + HEALTH_TIMEOUT_S
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"server exited early (code {self.proc.returncode}); see {self.log_path}"
                )
            try:
                status, body = http_json("GET", self.base_url + "/health", timeout=2.0)
                if status == 200 and isinstance(body, dict) and body.get("service") == "stt-server":
                    break
            except Exception:
                pass
            time.sleep(0.5)
        else:
            raise RuntimeError(f"server did not become healthy within {HEALTH_TIMEOUT_S}s")

        token_path = self.data_dir / "auth.token"
        deadline = time.monotonic() + 10.0
        while not token_path.exists() and time.monotonic() < deadline:
            time.sleep(0.2)
        self.token = token_path.read_text(encoding="utf-8").strip()

    def stop(self):
        if self.proc is None:
            return
        if self.proc.poll() is not None:
            return
        try:
            http_json(
                "POST",
                self.base_url + "/v1/local/shutdown",
                token=self.token,
                timeout=5.0,
            )
        except Exception:
            pass
        deadline = time.monotonic() + 15.0
        while self.proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.3)
        if self.proc.poll() is None:
            # last resort: the process wouldn't exit gracefully
            try:
                self.proc.terminate()
                self.proc.wait(timeout=5.0)
            except Exception:
                try:
                    self.proc.kill()
                except Exception:
                    pass


# ---------------------------------------------------------------------------
# Catalog
# ---------------------------------------------------------------------------

def get_catalog(server: Server):
    status, body = http_json("GET", server.base_url + "/models/manage", token=server.token, timeout=30.0)
    if status != 200:
        raise RuntimeError(f"failed to list models: {status} {body}")
    entries = body.get("data") if isinstance(body, dict) else body
    if entries is None:
        entries = body
    return entries


def advertised_capabilities(server: Server, model_id: str):
    """Install-time capability claims aren't reliable (catalog.rs's static
    view marks everything unsupported); the real, per-loaded-model matrix
    only exists after select, from GET /models/manage/default."""
    status, body = http_json(
        "GET", server.base_url + "/models/manage/default", token=server.token, timeout=30.0
    )
    if status != 200 or not isinstance(body, dict):
        return {}
    caps = body.get("effective_capabilities") or {}
    return caps


def is_supported(caps: dict, name: str) -> bool:
    entry = caps.get(name)
    return isinstance(entry, dict) and entry.get("status") == "supported"


# ---------------------------------------------------------------------------
# Operation polling
# ---------------------------------------------------------------------------

def run_operation(server: Server, method, path, timeout_s, payload=None, poll_every=OPERATION_POLL_INTERVAL_S):
    status, body = http_json(method, server.base_url + path, token=server.token, payload=payload, timeout=30.0)
    if status == 409 and isinstance(body, dict) and body.get("error", {}).get("code") == "already_installed":
        return {"state": "completed", "already": True}
    if status not in (200, 202):
        raise ApiError(status, body)
    op_id = body.get("operation_id") if isinstance(body, dict) else None
    if op_id is None:
        return body
    deadline = time.monotonic() + timeout_s
    last = None
    while time.monotonic() < deadline:
        ostatus, obody = http_json(
            "GET", server.base_url + f"/models/manage/operations/{op_id}", token=server.token, timeout=30.0
        )
        if ostatus != 200:
            raise ApiError(ostatus, obody)
        last = obody
        state = obody.get("state")
        if state in ("completed", "failed", "cancelled"):
            return obody
        time.sleep(poll_every)
    raise RuntimeError(f"operation {op_id} timed out after {timeout_s}s; last={last}")


# ---------------------------------------------------------------------------
# Clips
# ---------------------------------------------------------------------------

def load_clips(clips_dir: Path):
    manifest_path = clips_dir / "clips.json"
    if not manifest_path.exists():
        raise RuntimeError(f"clips manifest not found: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    clips = []
    for filename, reference in manifest.items():
        p = clips_dir / filename
        if not p.exists():
            raise RuntimeError(f"clip file missing: {p}")
        clips.append((p, reference))
    if not clips:
        raise RuntimeError(f"no clips in manifest: {manifest_path}")
    return clips


def make_clips(args):
    from_results = Path(args.from_results)
    clips_dir = Path(args.clips_dir)
    audio_root = Path(args.audio_root)
    clips_dir.mkdir(parents=True, exist_ok=True)

    candidates = []
    with open(from_results, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            wer_val = rec.get("wer")
            duration = rec.get("duration_sec")
            ref = rec.get("reference_text") or ""
            if wer_val is None or wer_val > 0.1:
                continue
            if duration is None or not (6.0 <= duration <= 15.0):
                continue
            if not ref.strip():
                continue
            # crude "clean English" check: mostly ascii letters/space/punct
            non_ascii = sum(1 for ch in ref if ord(ch) > 127)
            if non_ascii > 0:
                continue
            candidates.append(rec)

    if len(candidates) < 2:
        raise RuntimeError(
            f"only found {len(candidates)} candidate chunk(s) with wer<=0.1 and "
            f"6-15s duration in {from_results}; need at least 2"
        )

    candidates.sort(key=lambda r: r.get("wer", 1.0))
    chosen = candidates[:2]

    manifest = {}
    for rec in chosen:
        sid = rec["session_id"]
        cid = rec["chunk_id"]
        src = audio_root / str(sid) / f"{cid}.wav"
        if not src.exists():
            raise RuntimeError(f"source audio missing: {src}")
        dest_name = f"{sid}_{cid}.wav"
        shutil.copyfile(src, clips_dir / dest_name)
        manifest[dest_name] = rec.get("reference_text") or ""

    (clips_dir / "clips.json").write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8"
    )
    print(f"wrote {len(manifest)} clips + manifest to {clips_dir}")
    for name, ref in manifest.items():
        print(f"  {name}: {ref!r}")


# ---------------------------------------------------------------------------
# Sweep core
# ---------------------------------------------------------------------------

def load_done_keys(out_path: Path):
    done = set()
    if not out_path.exists():
        return done
    with open(out_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            done.add((rec.get("model"), rec.get("quant")))
    return done


def wer_status(wer_val):
    if wer_val is None:
        return "fail"
    if wer_val <= 0.5:
        return "ok"
    if wer_val <= 0.8:
        return "warn"
    return "fail"


def try_capability(server, endpoint, model_id, extra_fields, clip_path, non_english_only=False):
    fields = [("model", model_id)] + extra_fields
    status, parsed, err, wall_ms = http_multipart(
        server.base_url + endpoint, server.token, fields, clip_path, CAPABILITY_TIMEOUT_S
    )
    if err is not None:
        return {"ok": False, "error": err, "wall_ms": wall_ms}
    if status is None or status >= 400:
        return {"ok": False, "error": f"http_{status}", "wall_ms": wall_ms, "body": parsed}
    text = ""
    if isinstance(parsed, dict):
        text = parsed.get("text") or ""
    ok = bool(text.strip())
    return {"ok": ok, "wall_ms": wall_ms, "text": text[:200]}


def sweep_one(server, model, quant_file, clips, out_f, only_missing_output=True):
    model_id = model["slug"]
    quant = quant_file["quant"]
    filename = quant_file["filename"]
    size_bytes = quant_file["size_bytes"]

    record = {
        "model": model_id,
        "quant": quant,
        "filename": filename,
        "size_bytes": size_bytes,
        "status": "fail",
        "error": None,
        "install_s": None,
        "verify_s": None,
        "select_s": None,
        "backend": None,
        "clips": [],
        "capabilities": {},
    }

    installed = False
    selected = False
    try:
        # --- install ---
        t0 = time.monotonic()
        op = run_operation(
            server, "POST", f"/models/manage/{model_id}/download", INSTALL_TIMEOUT_S,
            payload={"quant": quant},
        )
        record["install_s"] = round(time.monotonic() - t0, 2)
        if op.get("state") not in ("completed",) and not op.get("already"):
            raise RuntimeError(f"install failed: {op}")
        installed = True

        # --- verify (needed if select reports needs_verification) ---
        status, sel_body = http_json(
            "POST", server.base_url + f"/models/manage/{model_id}/default",
            token=server.token, timeout=120.0,
        )
        if status == 409 and isinstance(sel_body, dict) and sel_body.get("error", {}).get("code") == "needs_verification":
            t0 = time.monotonic()
            vop = run_operation(server, "POST", f"/models/manage/{model_id}/verify", INSTALL_TIMEOUT_S)
            record["verify_s"] = round(time.monotonic() - t0, 2)
            if vop.get("state") != "completed":
                raise RuntimeError(f"verify failed: {vop}")
            status, sel_body = http_json(
                "POST", server.base_url + f"/models/manage/{model_id}/default",
                token=server.token, timeout=120.0,
            )
        if status != 200:
            raise ApiError(status, sel_body)
        selected = True
        record["backend"] = (sel_body or {}).get("backend")

        # --- capabilities as actually reported for the loaded model ---
        caps = advertised_capabilities(server, model_id)

        # --- transcribe sample clips ---
        # The clip manifest's reference text is always English (make_clips
        # rejects non-ASCII references), but many catalog models are not
        # English-capable at all. Scoring those against the English
        # reference by WER is meaningless -- a correct non-English
        # transcription of English audio "disagrees" with the reference in
        # every word, which used to score as "fail" for a model that is
        # actually working correctly. Score those by non-empty text only,
        # the same bar `try_capability` already uses.
        model_langs = model.get("languages") or []
        model_is_english_capable = not model_langs or "en" in model_langs
        for clip_path, reference in clips:
            t0 = time.monotonic()
            status, parsed, err, wall_ms = http_multipart(
                server.base_url + "/v1/audio/transcriptions",
                server.token,
                [("model", model_id)],
                clip_path,
                REQUEST_TIMEOUT_S,
            )
            hyp = ""
            if isinstance(parsed, dict):
                hyp = parsed.get("text") or ""
            w = wer(reference, hyp) if (reference and model_is_english_capable) else None
            clip_status = "fail"
            if err is None and status is not None and status < 400 and hyp.strip():
                clip_status = wer_status(w) if model_is_english_capable else "ok"
            record["clips"].append(
                {
                    "clip": clip_path.name,
                    "status": clip_status,
                    "wer": w,
                    "text": hyp[:300],
                    "wall_ms": wall_ms,
                    "http_status": status,
                    "error": err,
                }
            )

        # --- exercise each advertised capability once ---
        clip_path = clips[0][0]
        model_langs = model.get("languages") or []
        second_lang = next((l for l in model_langs if l != "en"), None)

        if is_supported(caps, "prompt"):
            record["capabilities"]["prompt"] = try_capability(
                server, "/v1/audio/transcriptions", model_id,
                [("prompt", "This is a test prompt for the sweep.")], clip_path,
            )
        if is_supported(caps, "language_hint"):
            lang_hint = model_langs[0] if model_langs else "en"
            record["capabilities"]["language_hint"] = try_capability(
                server, "/v1/audio/transcriptions", model_id,
                [("language", lang_hint)], clip_path,
            )
        if is_supported(caps, "translation"):
            result = try_capability(
                server, "/v1/audio/translations", model_id, [], clip_path,
            )
            record["capabilities"]["translation"] = result
        if is_supported(caps, "temperature"):
            record["capabilities"]["temperature"] = try_capability(
                server, "/v1/audio/transcriptions", model_id,
                [("temperature", "0.2")], clip_path,
            )
        if is_supported(caps, "timestamp_granularity"):
            granularity_entry = caps.get("timestamp_granularity") or {}
            max_granularity = (granularity_entry.get("extra") or {}).get("max")
            fields = [("response_format", "verbose_json"), ("timestamp_granularities", "segment")]
            if max_granularity == "word":
                fields.append(("timestamp_granularities", "word"))
            res_status, res_parsed, res_err, res_ms = http_multipart(
                server.base_url + "/v1/audio/transcriptions", server.token,
                [("model", model_id)] + fields, clip_path, CAPABILITY_TIMEOUT_S,
            )
            ok = False
            if res_err is None and res_status is not None and res_status < 400 and isinstance(res_parsed, dict):
                segs = res_parsed.get("segments") or []
                ok = len(segs) > 0
            record["capabilities"]["timestamp_granularity"] = {
                "ok": ok, "wall_ms": res_ms, "error": res_err,
                "http_status": res_status,
            }

        # --- overall status ---
        clip_statuses = [c["status"] for c in record["clips"]]
        cap_failures = [k for k, v in record["capabilities"].items() if not v.get("ok")]
        if any(s == "fail" for s in clip_statuses):
            record["status"] = "fail"
        elif cap_failures:
            record["status"] = "warn"
            record["error"] = f"capability failures: {cap_failures}"
        elif any(s == "warn" for s in clip_statuses):
            record["status"] = "warn"
        else:
            record["status"] = "ok"

    except Exception as e:  # noqa: BLE001
        record["status"] = "fail"
        record["error"] = str(e)
    finally:
        # always remove, even on failure, so only one model sits on disk
        try:
            if selected:
                http_json(
                    "DELETE", server.base_url + "/models/manage/default",
                    token=server.token, timeout=30.0,
                )
        except Exception:
            pass
        if installed:
            try:
                http_json(
                    "DELETE", server.base_url + f"/models/manage/{model_id}",
                    token=server.token, timeout=60.0,
                )
            except Exception as e:  # noqa: BLE001
                record["remove_error"] = str(e)

    out_f.write(json.dumps(record, ensure_ascii=False) + "\n")
    out_f.flush()
    return record


def print_progress(record):
    clip_werstr = ", ".join(
        f"{c['clip']}={c['wer']:.2f}" if c.get("wer") is not None else f"{c['clip']}=NA"
        for c in record["clips"]
    )
    caps = record["capabilities"]
    cap_str = ", ".join(f"{k}={'ok' if v.get('ok') else 'FAIL'}" for k, v in caps.items()) or "-"
    print(
        f"[{record['status'].upper():4}] {record['model']}/{record['quant']} "
        f"install={record['install_s']}s clips=[{clip_werstr}] caps=[{cap_str}] "
        f"err={record.get('error')}",
        flush=True,
    )


def write_markdown_summary(out_path: Path, md_path: Path):
    rows = []
    with open(out_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    lines = [
        "# Catalog sweep results",
        "",
        f"Total entries: {len(rows)}",
        "",
        "| Model | Quant | Status | Install (s) | Clip WERs | Capabilities | Error |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in rows:
        clip_wers = ", ".join(
            f"{c['wer']:.2f}" if c.get("wer") is not None else "NA" for c in r.get("clips", [])
        )
        caps = r.get("capabilities", {})
        cap_str = ", ".join(f"{k}:{'ok' if v.get('ok') else 'fail'}" for k, v in caps.items()) or "-"
        err = (r.get("error") or "").replace("|", "/")[:120]
        lines.append(
            f"| {r.get('model')} | {r.get('quant')} | {r.get('status')} | "
            f"{r.get('install_s')} | {clip_wers} | {cap_str} | {err} |"
        )

    ok = sum(1 for r in rows if r.get("status") == "ok")
    warn = sum(1 for r in rows if r.get("status") == "warn")
    fail = sum(1 for r in rows if r.get("status") == "fail")
    lines += ["", f"ok={ok} warn={warn} fail={fail}"]

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--exe", default=None, help="server exe path (default: debug build, falls back to release)")
    parser.add_argument("--data-dir", default=DEFAULT_DATA_DIR)
    parser.add_argument("--port", type=int, default=None, help="default: first free port from 54500")
    parser.add_argument("--clips-dir", default=None, help="dir with WAVs + clips.json")
    parser.add_argument("--out", default="catalog_sweep.results.jsonl")
    parser.add_argument("--summary-md", default=None, help="default: <out>.md")
    parser.add_argument("--only", default=None, help="substring filter on model slug")
    parser.add_argument("--limit", type=int, default=None, help="stop after N model/quant combos")
    parser.add_argument("--all-quants", action="store_true", help="test every quant, not only each model's default_quant")
    parser.add_argument("--redo", action="store_true", help="re-run entries already in --out")
    parser.add_argument("--log", default=None, help="server stdout/stderr log path (default: <data-dir>/server.log)")
    parser.add_argument(
        "--download-timeout", type=float, default=DEFAULT_DOWNLOAD_TIMEOUT_S,
        help=f"seconds to wait for an install/verify operation (default: {DEFAULT_DOWNLOAD_TIMEOUT_S:.0f})",
    )

    parser.add_argument("--make-clips", action="store_true", help="build a clips dir instead of sweeping")
    parser.add_argument("--from-results", default=None, help="bench_corpus.py results jsonl (for --make-clips)")
    parser.add_argument("--audio-root", default=DEFAULT_AUDIO_ROOT)

    args = parser.parse_args()

    global INSTALL_TIMEOUT_S
    INSTALL_TIMEOUT_S = args.download_timeout

    if args.make_clips:
        if not args.from_results or not args.clips_dir:
            parser.error("--make-clips requires --from-results and --clips-dir")
        make_clips(args)
        return

    if not args.clips_dir:
        parser.error("--clips-dir is required")

    exe = args.exe
    if exe is None:
        exe = DEFAULT_EXE_DEBUG if Path(DEFAULT_EXE_DEBUG).exists() else DEFAULT_EXE_RELEASE
    if not Path(exe).exists():
        print(f"ERROR: server exe not found: {exe}", file=sys.stderr)
        sys.exit(1)

    data_dir = Path(args.data_dir)
    port = args.port or find_spare_port()
    log_path = Path(args.log) if args.log else data_dir / "server.log"
    data_dir.mkdir(parents=True, exist_ok=True)

    clips_dir = Path(args.clips_dir)
    clips = load_clips(clips_dir)

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    md_path = Path(args.summary_md) if args.summary_md else out_path.with_suffix(out_path.suffix + ".md")

    done = set() if args.redo else load_done_keys(out_path)

    server = Server(exe, data_dir, port, log_path)

    stop_requested = {"flag": False}

    def handle_sigint(signum, frame):
        stop_requested["flag"] = True
        print("\nCtrl+C received, stopping server and exiting after current model...", flush=True)

    signal.signal(signal.SIGINT, handle_sigint)

    print(f"starting server: {exe} --port {port} --data-dir {data_dir}", flush=True)
    server.start()
    print(f"server up at {server.base_url}, log={log_path}", flush=True)

    try:
        catalog = get_catalog(server)

        work = []
        for model in catalog:
            slug = model.get("id") or model.get("slug")
            if args.only and args.only not in slug:
                continue
            files = model.get("files") or []
            if not args.all_quants and model.get("default_quant"):
                files = [f for f in files if f.get("quant") == model["default_quant"]] or files[:1]
            for file in files:
                work.append((model, file))

        print(f"catalog: {len(catalog)} models, {len(work)} model/quant combos to consider", flush=True)

        # Need full model dicts (with slug/languages) from catalog for internal use;
        # /models/manage view may already have flattened fields — normalise.
        normalized_work = []
        for model, file in work:
            slug = model.get("id") or model.get("slug")
            languages = model.get("languages") or []
            quant = file.get("quant")
            key = (slug, quant)
            if key in done:
                continue
            normalized_work.append(({"slug": slug, "languages": languages}, file))

        if args.limit is not None:
            normalized_work = normalized_work[: args.limit]

        already_done = sum(1 for m, f in work if ((m.get("id") or m.get("slug")), f.get("quant")) in done)
        print(f"running {len(normalized_work)} combos (already done: {already_done})", flush=True)

        with open(out_path, "a", encoding="utf-8") as out_f:
            for model, file in normalized_work:
                if stop_requested["flag"]:
                    break
                record = sweep_one(server, model, file, clips, out_f)
                print_progress(record)

    finally:
        print("stopping server...", flush=True)
        server.stop()
        write_markdown_summary(out_path, md_path)
        print(f"summary written to {md_path}", flush=True)


if __name__ == "__main__":
    main()
