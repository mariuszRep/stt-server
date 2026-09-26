#!/usr/bin/env python3
"""Benchmark harness for running the stt-server-next transcription API over a
corpus of recorded voice-typer sessions.

Python 3 stdlib only. See CLAUDE.md task spec for details.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time
import urllib.request
import urllib.error
import uuid
from pathlib import Path


# ---------------------------------------------------------------------------
# Text normalization + edit distance
# ---------------------------------------------------------------------------

_CURLY_APOS = {
    "‘": "'",
    "’": "'",
    "ʼ": "'",
    "′": "'",
}


def normalize_text(text: str) -> str:
    """Lowercase, map curly apostrophes to ', strip punctuation except
    intra-word apostrophes, collapse whitespace."""
    if text is None:
        return ""
    s = text.lower()
    for curly, straight in _CURLY_APOS.items():
        s = s.replace(curly, straight)

    out_chars = []
    n = len(s)
    for i, ch in enumerate(s):
        if ch.isalnum():
            out_chars.append(ch)
        elif ch == "'":
            # keep only if intra-word: alnum on both sides
            prev_ok = i > 0 and s[i - 1].isalnum()
            next_ok = i + 1 < n and s[i + 1].isalnum()
            if prev_ok and next_ok:
                out_chars.append(ch)
            else:
                out_chars.append(" ")
        elif ch.isspace():
            out_chars.append(" ")
        else:
            out_chars.append(" ")
    s = "".join(out_chars)
    s = " ".join(s.split())
    return s


def edit_distance(a: list, b: list) -> int:
    """Iterative DP Levenshtein distance between two sequences."""
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
            curr[j] = min(
                prev[j] + 1,      # deletion
                curr[j - 1] + 1,  # insertion
                prev[j - 1] + cost,  # substitution
            )
        prev, curr = curr, prev
    return prev[m]


def word_edit_distance(ref_norm: str, hyp_norm: str) -> tuple:
    ref_words = ref_norm.split()
    hyp_words = hyp_norm.split()
    dist = edit_distance(ref_words, hyp_words)
    return dist, len(ref_words)


def char_edit_distance(ref_norm: str, hyp_norm: str) -> tuple:
    ref_chars = list(ref_norm.replace(" ", ""))
    hyp_chars = list(hyp_norm.replace(" ", ""))
    dist = edit_distance(ref_chars, hyp_chars)
    return dist, len(ref_chars)


# ---------------------------------------------------------------------------
# Self test
# ---------------------------------------------------------------------------

def run_selftest() -> None:
    # normalization
    assert normalize_text("Hello, World!") == "hello world"
    assert normalize_text("It’s a test.") == "it's a test"
    assert normalize_text("  multi   space  ") == "multi space"
    assert normalize_text("don't stop") == "don't stop"
    assert normalize_text("'quoted' word") == "quoted word"
    assert normalize_text("") == ""
    assert normalize_text("Café résumé") == "café résumé"  # unicode alnum kept

    # edit distance
    assert edit_distance(list("kitten"), list("sitting")) == 3
    assert edit_distance(list(""), list("abc")) == 3
    assert edit_distance(list("abc"), list("abc")) == 0
    assert edit_distance([], []) == 0

    # word/char wer helpers
    d, n = word_edit_distance("the cat sat", "the cat sat")
    assert d == 0 and n == 3
    d, n = word_edit_distance("the cat sat", "the dog sat")
    assert d == 1 and n == 3
    d, n = char_edit_distance("cat", "cats")
    assert d == 1 and n == 3

    print("selftest: OK", flush=True)


# ---------------------------------------------------------------------------
# Multipart request building
# ---------------------------------------------------------------------------

def build_multipart(fields: list, file_field_name: str, file_path: Path):
    boundary = uuid.uuid4().hex
    parts = []
    crlf = b"\r\n"

    for name, value in fields:
        parts.append(b"--" + boundary.encode() + crlf)
        parts.append(
            f'Content-Disposition: form-data; name="{name}"'.encode() + crlf
        )
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
    content_type = f"multipart/form-data; boundary={boundary}"
    return body, content_type


def do_transcribe_request(
    server: str,
    endpoint: str,
    model: str,
    extra_fields: list,
    token: str,
    audio_path: Path,
    timeout_s: float = 120.0,
):
    """Returns (status_code, parsed_json_or_none, raw_text, wall_ms, error_str)."""
    fields = [("model", model)] + list(extra_fields)
    body, content_type = build_multipart(fields, "file", audio_path)

    url = server.rstrip("/") + endpoint
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", content_type)
    req.add_header("Content-Length", str(len(body)))
    if token:
        req.add_header("Authorization", f"Bearer {token}")

    start = time.monotonic()
    status_code = None
    raw_text = ""
    error_str = None
    parsed = None
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            status_code = resp.status
            raw_text = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        status_code = e.code
        try:
            raw_text = e.read().decode("utf-8", errors="replace")
        except Exception:
            raw_text = ""
        error_str = f"HTTPError {e.code}"
    except urllib.error.URLError as e:
        error_str = f"URLError {e.reason}"
    except Exception as e:  # noqa: BLE001
        error_str = f"Exception {e!r}"
    wall_ms = (time.monotonic() - start) * 1000.0

    if raw_text:
        try:
            parsed = json.loads(raw_text)
        except json.JSONDecodeError:
            parsed = None

    return status_code, parsed, raw_text, wall_ms, error_str


# ---------------------------------------------------------------------------
# Corpus loading
# ---------------------------------------------------------------------------

def load_sessions(sessions_path: Path):
    with open(sessions_path, "r", encoding="utf-8") as f:
        return json.load(f)


def iter_chunks(sessions):
    for session in sessions:
        sid = session.get("id")
        for chunk in session.get("chunks", []):
            yield sid, chunk


# ---------------------------------------------------------------------------
# Stats helpers
# ---------------------------------------------------------------------------

def percentile(values: list, pct: float):
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (pct / 100.0)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def median(values: list):
    if not values:
        return None
    return statistics.median(values)


def duration_bucket(duration_sec):
    if duration_sec is None:
        return "unknown"
    if duration_sec < 3:
        return "<3s"
    if duration_sec < 10:
        return "3-10s"
    if duration_sec < 30:
        return "10-30s"
    return "30s+"


# ---------------------------------------------------------------------------
# Main run
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    default_sessions = os.path.expandvars(
        r"%APPDATA%\com.voicetyper.desktop\sessions.json"
    )
    default_audio_root = os.path.expandvars(
        r"%APPDATA%\com.voicetyper.desktop\session-audio"
    )
    parser.add_argument("--sessions", default=default_sessions)
    parser.add_argument("--audio-root", default=default_audio_root)
    parser.add_argument("--server", default="http://127.0.0.1:54321")
    parser.add_argument("--token-file", default=None)
    parser.add_argument("--endpoint", default="/v1/audio/transcriptions")
    parser.add_argument("--model", default="default")
    parser.add_argument(
        "--field",
        action="append",
        default=[],
        help="Extra multipart field k=v (repeatable)",
    )
    parser.add_argument("--out", default=".")
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--label", default="run")
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()

    if args.selftest:
        run_selftest()
        return

    extra_fields = []
    for f in args.field:
        if "=" not in f:
            print(f"WARNING: ignoring malformed --field {f!r}", file=sys.stderr)
            continue
        k, v = f.split("=", 1)
        extra_fields.append((k, v))

    token = None
    if args.token_file:
        with open(args.token_file, "r", encoding="utf-8") as f:
            token = f.read().strip()

    sessions_path = Path(args.sessions)
    audio_root = Path(args.audio_root)
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    sessions = load_sessions(sessions_path)

    results_path = out_dir / f"{args.label}.results.jsonl"
    summary_path = out_dir / f"{args.label}.summary.json"

    skipped_missing_wav = 0
    failures_by_code = {}
    per_chunk_records = []

    first_request_latency_ms = None
    processed = 0

    with open(results_path, "w", encoding="utf-8") as results_f:
        for sid, chunk in iter_chunks(sessions):
            if args.limit is not None and processed >= args.limit:
                break

            cid = chunk.get("id")
            duration_sec = chunk.get("durationSec")
            reference = chunk.get("transcript") or ""

            audio_path = audio_root / str(sid) / f"{cid}.wav"
            if not audio_path.exists():
                skipped_missing_wav += 1
                continue

            processed += 1

            status_code, parsed, raw_text, wall_ms, error_str = do_transcribe_request(
                args.server,
                args.endpoint,
                args.model,
                extra_fields,
                token,
                audio_path,
            )

            if first_request_latency_ms is None:
                first_request_latency_ms = wall_ms

            hyp_text = ""
            error_code = None
            diagnostics = None

            if parsed is not None:
                if isinstance(parsed, dict):
                    if "error" in parsed and isinstance(parsed["error"], dict):
                        error_code = parsed["error"].get("code")
                    hyp_text = parsed.get("text") or ""
                    diag = parsed.get("x_diagnostics")
                    if isinstance(diag, dict):
                        diagnostics = diag

            if error_str is not None and error_code is None:
                error_code = error_str

            if status_code is None or (status_code and status_code >= 400):
                code_key = error_code or (str(status_code) if status_code else "unknown")
                failures_by_code[code_key] = failures_by_code.get(code_key, 0) + 1

            ref_norm = normalize_text(reference)
            hyp_norm = normalize_text(hyp_text)

            word_dist, ref_word_count = word_edit_distance(ref_norm, hyp_norm)
            char_dist, ref_char_count = char_edit_distance(ref_norm, hyp_norm)

            wer = (word_dist / ref_word_count) if ref_word_count > 0 else None
            cer = (char_dist / ref_char_count) if ref_char_count > 0 else None
            exact_match = ref_norm == hyp_norm

            inference_ms = diagnostics.get("inference_ms") if diagnostics else None
            queue_wait_ms = diagnostics.get("queue_wait_ms") if diagnostics else None
            audio_ms = diagnostics.get("audio_ms") if diagnostics else None
            backend = diagnostics.get("backend") if diagnostics else None

            if inference_ms is not None and audio_ms:
                rtf = inference_ms / audio_ms
            elif duration_sec:
                rtf = (wall_ms / 1000.0) / duration_sec
            else:
                rtf = None

            record = {
                "session_id": sid,
                "chunk_id": cid,
                "duration_sec": duration_sec,
                "wall_latency_ms": wall_ms,
                "http_status": status_code,
                "error_code": error_code,
                "reference_text": reference,
                "hypothesis_text": hyp_text,
                "word_edit_distance": word_dist,
                "ref_word_count": ref_word_count,
                "char_edit_distance": char_dist,
                "ref_char_count": ref_char_count,
                "wer": wer,
                "cer": cer,
                "exact_match": exact_match,
                "rtf": rtf,
                "inference_ms": inference_ms,
                "queue_wait_ms": queue_wait_ms,
                "audio_ms": audio_ms,
                "backend": backend,
            }
            per_chunk_records.append(record)
            results_f.write(json.dumps(record, ensure_ascii=False) + "\n")
            results_f.flush()

            if processed % 25 == 0:
                print(f"progress: {processed} chunks processed", flush=True)

    # Aggregate summary
    total_word_edits = sum(r["word_edit_distance"] for r in per_chunk_records)
    total_ref_words = sum(r["ref_word_count"] for r in per_chunk_records)
    total_char_edits = sum(r["char_edit_distance"] for r in per_chunk_records)
    total_ref_chars = sum(r["ref_char_count"] for r in per_chunk_records)

    corpus_wer = (total_word_edits / total_ref_words) if total_ref_words > 0 else None
    corpus_cer = (total_char_edits / total_ref_chars) if total_ref_chars > 0 else None

    exact_match_count = sum(1 for r in per_chunk_records if r["exact_match"])
    exact_match_pct = (
        (exact_match_count / len(per_chunk_records) * 100.0)
        if per_chunk_records
        else None
    )

    latencies = [r["wall_latency_ms"] for r in per_chunk_records]
    rtfs = [r["rtf"] for r in per_chunk_records if r["rtf"] is not None]

    bucket_stats = {}
    for bucket_name in ["<3s", "3-10s", "10-30s", "30s+", "unknown"]:
        bucket_records = [
            r for r in per_chunk_records
            if duration_bucket(r["duration_sec"]) == bucket_name
        ]
        if not bucket_records:
            continue
        b_word_edits = sum(r["word_edit_distance"] for r in bucket_records)
        b_ref_words = sum(r["ref_word_count"] for r in bucket_records)
        b_wer = (b_word_edits / b_ref_words) if b_ref_words > 0 else None
        b_latencies = [r["wall_latency_ms"] for r in bucket_records]
        bucket_stats[bucket_name] = {
            "count": len(bucket_records),
            "wer": b_wer,
            "median_latency_ms": median(b_latencies),
        }

    summary = {
        "label": args.label,
        "count": len(per_chunk_records),
        "skipped_missing_wav": skipped_missing_wav,
        "failures_by_code": failures_by_code,
        "corpus_wer": corpus_wer,
        "corpus_cer": corpus_cer,
        "exact_match_pct": exact_match_pct,
        "latency_ms": {
            "median": median(latencies),
            "p90": percentile(latencies, 90),
            "p99": percentile(latencies, 99),
        },
        "rtf": {
            "median": median(rtfs),
            "p90": percentile(rtfs, 90),
            "p99": percentile(rtfs, 99),
        },
        "first_request_latency_ms": first_request_latency_ms,
        "duration_buckets": bucket_stats,
    }

    with open(summary_path, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2, ensure_ascii=False)

    print(f"done: {len(per_chunk_records)} chunks, results={results_path}, summary={summary_path}", flush=True)


if __name__ == "__main__":
    main()
