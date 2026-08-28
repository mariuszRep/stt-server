"""Download-only entry point for a model's weights.

Invoked as `<runtime> download-model <model-id> <output-dir>` by
`crates/runtime/src/providers/faster_whisper.rs::download_model` — no HTTP
server is started and no CTranslate2 model is constructed, so this needs
neither a free port nor CUDA/device resolution, just the underlying
HuggingFace fetch `faster_whisper.download_model()` already wraps.
"""

import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: download-model <model-id> <output-dir>", file=sys.stderr)
        return 2

    model_id, output_dir = argv
    from faster_whisper import download_model

    try:
        download_model(model_id, output_dir=output_dir)
    except Exception as exc:
        print(f"[voice-typer] model download failed: {exc}", file=sys.stderr)
        return 1

    print(f"[voice-typer] model {model_id} downloaded to {output_dir}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
