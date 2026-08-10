# stt-server

A self-hosted, vendor-neutral Speech-to-Text (STT) platform.

## Features

- **Batch transcription**: Submit WAV PCM audio via HTTP POST, get normalized text/segments/duration.
- **Realtime transcription**: Stream 16kHz mono s16le PCM frames via WebSocket, receive partial/final events.
- **Model lifecycle**: CLI/API for model discovery, download, verification, listing, selection, loading/unloading.
- **Health/readiness**: HTTP endpoints for operational probing.
- **Minimal SDK**: Typed TypeScript/JavaScript client for batch and realtime transcription.

## Architecture

```
stt-server/
├── crates/
│   ├── common/    # Shared types, errors, config
│   ├── adapter/   # Canonical engine adapter trait + whisper.cpp implementation
│   ├── server/    # HTTP/WS server (axum)
│   └── cli/       # CLI commands
├── sdk/           # TypeScript/JavaScript SDK
├── fixtures/      # Test audio fixtures
└── docker/        # Dockerfiles
```

## Quick Start

```bash
# Build
cargo build --release

# Run server
./target/release/stt-server --port 8080

# Health check
curl http://127.0.0.1:8080/v1/health
```

## API

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/health` | Health check |
| GET | `/v1/readiness` | Readiness check |
| GET | `/v1/models` | List models |
| GET | `/v1/models/selected` | Get selected model |
| POST | `/v1/models/select` | Select default model |
| POST | `/v1/transcriptions` | Batch transcription (WAV body) |
| WS | `/v1/realtime/transcriptions` | Realtime transcription |

### WebSocket Protocol

Messages are JSON with a `type` field:

**Client → Server:**
- `{ "type": "start", "config": {...} }` — Start session
- `{ "type": "binary", "data": [...] }` — Audio data (base64)
- Binary frames — Raw PCM data
- `{ "type": "complete" }` — End session
- `{ "type": "cancel" }` — Cancel session

**Server → Client:**
- `{ "type": "started", "session_id": "..." }` — Session started
- `{ "type": "partial", "text": "..." }` — Partial result
- `{ "type": "final", "text": "...", "segments": [...] }` — Final result
- `{ "type": "completed", "session_id": "..." }` — Session completed
- `{ "type": "error", "code": "...", "message": "..." }` — Error

## CLI

```bash
# Start server
stt run --port 8080

# Model management
stt model list
stt model pull <model-id>
stt model remove <model-id>
stt model select <model-id>
stt model verify <path>
```

## SDK

```typescript
import { SttClient } from '@stt/server-sdk';

const client = new SttClient({ baseUrl: 'http://127.0.0.1:8080' });

// Health check
const health = await client.health();

// Batch transcription
const result = await client.transcribe(wavBuffer, { language: 'en' });

// Realtime transcription
const session = client.realtime({ sample_rate: 16000, channels: 1, sample_format: 'signed_16bit_le' });
await session.connect();
session.on('partial', (msg) => console.log('Partial:', msg.text));
session.on('final', (msg) => console.log('Final:', msg.text));
session.sendAudio(audioSamples);
session.complete();
```

## License

MIT
