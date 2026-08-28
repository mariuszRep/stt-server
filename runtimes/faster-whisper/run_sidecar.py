import sys

if __name__ == "__main__":
    # A separate download-only mode (no HTTP server, no CUDA/device
    # resolution) checked before anything else is imported, so it pays no
    # cost from the hardware/CUDA probing `app.config` does at import time.
    if len(sys.argv) > 1 and sys.argv[1] == "download-model":
        from app.download import main as download_main

        sys.exit(download_main(sys.argv[2:]))

    import time

    import uvicorn
    from app import config

    # On a restart handoff (Tauri stops the old sidecar, then spawns this one a
    # moment later) the previous process's listening socket can take a beat to
    # release on Windows, so uvicorn's bind_socket() fails with WinError 10048
    # even though the port is about to be free. Retry instead of crashing.
    max_attempts = 20
    for attempt in range(1, max_attempts + 1):
        try:
            uvicorn.run("app.main:app", host=config.HOST, port=config.PORT)
            break
        except SystemExit:
            if attempt == max_attempts:
                raise
            time.sleep(0.25)
