import time

import uvicorn
from app import config

if __name__ == "__main__":
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
