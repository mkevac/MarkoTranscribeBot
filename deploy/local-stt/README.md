# Local Whisper transcription server

Run this stack on a separate Linux server with Docker Compose. The tested
deployment uses 4 CPU cores, 8 GB RAM, and a 32 GB disk. It runs Speaches v0.8.2
with faster-whisper, CPU INT8 inference, and Whisper large-v3-turbo. The image is
pinned by digest; model files persist in a Docker volume.

## Install

Copy `docker-compose.yml` to `/opt/Speaches/`, copy `.env.example` to
`/opt/Speaches/.env`, and set the server's private LAN address. Generate a separate
API key with `python3 -c 'import secrets; print(secrets.token_urlsafe(32))'` and put
it in that private `.env` file. Restrict the file with `chmod 600 .env`.

From `/opt/Speaches`:

```bash
docker compose pull
docker compose up -d --no-build --wait --wait-timeout 120
docker compose exec -T speaches python - <<'PY'
import os, urllib.request
request = urllib.request.Request(
    'http://127.0.0.1:8000/v1/models/deepdml/faster-whisper-large-v3-turbo-ct2',
    method='POST',
    headers={'Authorization': 'Bearer ' + os.environ['API_KEY']},
)
with urllib.request.urlopen(request, timeout=600) as response:
    print('Model download:', response.status)
PY
```

The download is approximately 1.6 GB. The first transcription loads the model;
`WHISPER__TTL=-1` keeps it in memory until the service stops. Health checks verify
the authenticated HTTP service; they do not load the model.

Install `Speaches.service` in `/etc/systemd/system/`, then run
`systemctl daemon-reload` and `systemctl enable --now Speaches`. Docker should also
be enabled at boot. Use `systemctl stop Speaches` to stop the stack.

## Connect the bot

Back up the bot's private configuration, then set these values in its `.env`:

```dotenv
STT_PROVIDER=openai
STT_BASE_URL=http://192.168.100.25:8000/v1
STT_MODEL=deepdml/faster-whisper-large-v3-turbo-ct2
STT_API_KEY=the-separate-local-api-key
CHUNK_SECONDS=120
STT_TIMEOUT_SECONDS=300
```

Use the actual server address and key. Recreate only the bot worker:
`docker compose up -d --no-deps --no-build markotranscribebot`.
No bot image rebuild is required. The bot still converts recordings to mono
16 kHz OGG/Opus and sends OpenAI-compatible multipart requests. Long recordings
use two-minute chunks to keep CPU processing within the request timeout.

The endpoint uses HTTP on a trusted private LAN. Keep port 8000 off the public
internet; use a VPN or TLS when crossing an untrusted network. Reserve the server
address in DHCP. If its address changes, update `STT_BIND_IP` on the service and
`STT_BASE_URL` on the bot and recreate the affected containers.

`STT_LANGUAGE=auto` enables language detection. For better results on short
Serbian or other ambiguous recordings, users can explicitly select `/lang sr`
or `/lang ru`. Local Whisper can differ in accuracy from the cloud model.

To roll back, restore the bot's previous `STT_*` and `CHUNK_SECONDS` settings from
the private backup and recreate the worker. There is no automatic cloud fallback.

## Verification on 2026-10-21

Three cached Telegram recordings of 4.2, 58.5, and 59.7 seconds transcribed in
10.9, 18.5, and 24.1 seconds respectively with automatic language detection.
These timings cover the transcription request, excluding media conversion and
Telegram transfer. Successful decoding and nonempty output establish compatibility;
they do not establish transcription accuracy. Synthetic Russian and Serbian tests
had errors, and automatic detection selected Slovak for the synthetic Serbian voice.

The installed model revision was
`4df90f75321148c3a29a9e2351b7ddf8f5b115a8`. The image contains faster-whisper
1.1.1 and CTranslate2 4.5.0.
