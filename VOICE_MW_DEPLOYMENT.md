# VoiceMW — Deployment and Operations Guide

VoiceMW is a customer-facing text-to-speech and speech-to-text service built around the Kokoro 82M model and a separate Whisper transcription service.

This guide documents the application architecture, runtime requirements, account administration, and operational considerations.

## 1. Architecture

The production deployment consists of four main components:

| Component | Responsibility |
|---|---|
| Kokoro API | Rust HTTP API, TTS synthesis, customer accounts, authentication and usage tracking |
| Whisper STT | Separate whisper.cpp HTTP transcription service |
| VoiceMW Web | Browser-based customer workspace and administrator interface |
| Public documentation | Customer onboarding and API documentation |

### Request flow

- Customers access the VoiceMW web application through HTTPS.
- The web application communicates with the Kokoro API through HTTP endpoints.
- The API uses Kokoro for speech synthesis.
- The API forwards transcription requests to the private Whisper service.
- Both services share the `kokoro-internal` Docker network.
- Customer accounts, subscriptions, API keys, passwords and browser sessions are stored in SQLite.

### Production network

The production API listens on port `8080`, bound to `127.0.0.1` on the host.

Whisper listens on port `8081` inside Docker only. It has no published host port.

The internal service address is:

`http://whisper-stt:8081/inference`

The API and Whisper containers must share the `kokoro-internal` Docker network.

## 2. Repository structure

The repository contains:

- `src/` — Rust library, API, account system and administration CLI.
- `web/` — VoiceMW customer and administrator browser workspace.
- `docs/` — Public customer documentation.
- `models/` — Kokoro model weights, downloaded separately.
- `voices/` — Voice assets, downloaded separately.
- `data/` — Runtime database and administrator configuration.
- `Cargo.toml` and `Cargo.lock` — Rust package configuration and locked dependencies.

Runtime model files, voice binaries, databases and credentials are deliberately excluded from Git.

## 3. Requirements

### Kokoro API

Production uses Ubuntu 24.04. Required components:

- Compatible Rust toolchain.
- CMake, C/C++ compiler and pkg-config.
- FFmpeg and FFprobe.
- Kokoro ONNX model.
- At least one `.bin` voice file.
- Writable persistent data directory.

Production inference uses:

`KOKORO_ORT_PROVIDER=cpu`

The model is loaded from `models/model_quantized.onnx`, and voice assets are loaded from `voices/`.

See `README.md` for model and voice download instructions.

### Whisper STT

Whisper is a separate dependency based on [whisper.cpp](https://github.com/ggml-org/whisper.cpp).

Production configuration:

- Model: `ggml-base.bin`
- Internal port: `8081`
- Threads: `2`
- Processors: `1`
- Service URL: `http://whisper-stt:8081/inference`

Whisper and the Kokoro API must share the `kokoro-internal` Docker network.

Whisper source, model weights and temporary files are maintained separately from this repository.

## 4. API configuration

| Variable | Purpose |
|---|---|
| `KOKORO_DB_PATH` | SQLite database location |
| `KOKORO_ORT_PROVIDER` | ONNX Runtime inference provider |
| `KOKORO_ADMIN_CONFIG` | Administrator authentication file |

Production uses CPU inference:

`KOKORO_ORT_PROVIDER=cpu`

The API listens on port `8080` inside its runtime environment.

The database defaults to `data/kokoro.db`, relative to the working directory, unless `KOKORO_DB_PATH` is set.

The administrator configuration defaults to `/workspace/data/admin-auth.txt`, unless `KOKORO_ADMIN_CONFIG` is set.

Keep the database, administrator configuration and all credentials outside version control.

## 5. Building the Rust application

The repository defines four binaries:

- `kokoro-api`: HTTP API and browser backend.
- `kokoro-admin`: Account administration CLI.
- `kokoro-wav`: Audio synthesis example.
- `kokoro-g2p-audit`: Pronunciation auditing.
