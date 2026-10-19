# Marko Transcribe Bot

A Telegram bot that turns voice messages, video circles and audio/video files into text, using your own speech-to-text provider and API key: OpenAI, Groq, ElevenLabs, or a self-hosted Whisper server.

Telegram's built-in transcription can't be configured. With this bot you pick the model, the language and a prompt with names and jargon, so the text comes out the way you want.

## Three ways to use it

| Mode | How | Who sees the transcript |
|---|---|---|
| Private chat | Send or forward a voice message, circle, or audio/video file to the bot. | Only you. It arrives as a reply under the file. |
| Guest mode | In any chat, reply to a voice message or circle with `@YourBot`. | Everyone in that chat. The bot posts one reply. |
| Business (automatic) | Telegram → Settings → Telegram Business → Chatbots → add the bot, then choose chats. | Only you. Incoming voice messages and circles are transcribed automatically and sent to your chat with the bot. The other person sees nothing. |

Business mode requires Telegram Business (part of Telegram Premium). By default it ignores your own outgoing messages (`BUSINESS_INCLUDE_OWN=true` changes that).

## How it works

```
Telegram update ──► queue (one job at a time, Cancel button, "Queued at #N")
                      │
                      ├─ getFile (local Bot API server: read file in place; cloud: download)
                      ├─ ffmpeg: extract audio → mono 16 kHz Opus, ~24 kbit/s
                      ├─ ffmpeg: split into CHUNK_SECONDS pieces if long
                      ├─ speech-to-text per piece (previous piece's tail is used as a prompt)
                      └─ reply: one message, or transcript.txt if longer than ~4000 chars
```

The bot calls the Telegram Bot API directly with `reqwest` and `serde` rather than through a bot framework, so newer features (guest mode, business messages) work without waiting on a library.

## Installation

Prebuilt images are available on [Docker Hub](https://hub.docker.com/r/mkevac/markotranscribebot)
for `linux/amd64` and `linux/arm64`. The first release is
`mkevac/markotranscribebot:0.1.0`; `latest` tracks the newest published release.
The Compose file uses `latest`; change its bot image tag to pin a specific version.

1. Create a bot with [BotFather](https://t.me/BotFather) and keep the token.
2. In BotFather's Mini App → your bot → Bot Settings:
   - turn on **Guest Mode** (for `@YourBot` replies in any chat);
   - turn on **Secretary Mode / Business** (for automatic transcription).
3. Get an API ID and hash from https://my.telegram.org/apps (needed for the local Bot API server).
4. Copy `.env.example` to `.env` and fill it in. At minimum: `TELEGRAM_BOT_API_TOKEN`, `ADMIN_CHAT_ID`, `STT_API_KEY`.
5. Log the bot out of the cloud Bot API once: `just logout-cloud`. Telegram requires this before a bot runs on a local server, and the bot can't log back in to the cloud server for 10 minutes.
6. Pull the images with `docker compose pull`, then start with `docker compose up -d --no-build`.
7. Send `/start` to your bot.

Your Telegram user ID is the same as your private chat ID with the bot. If you don't know it, message the bot from that account: it replies with your ID and reports it to the admin.

### Server deployment

Use `deploy/docker-compose.yml` as `/opt/MarkoTranscribeBot/docker-compose.yml`
and place `.env` beside it. This configuration uses the published `0.1.0` image,
keeps the Bot API server on the internal Docker network, and restarts both services
automatically. Set `TRANSCRIBE_IMAGE_TAG` in `.env` to select a later release.

Install `deploy/MarkoTranscribeBot.service` in `/etc/systemd/system/`, pull the
images from `/opt/MarkoTranscribeBot`, then run
`systemctl daemon-reload` and `systemctl enable --now MarkoTranscribeBot`.
Use `systemctl stop MarkoTranscribeBot` to stop the stack.

### Without the local Bot API server

Leave `BOT_API_URL` unset and run only the bot container (or `just run-local`). Everything works, except that Telegram's cloud API only lets bots download files up to 20 MB. That covers circles and voice messages, but not large audio files.

## Speech-to-text providers

| Provider | Settings |
|---|---|
| OpenAI (default) | `STT_PROVIDER=openai`, `STT_MODEL=gpt-transcribe` (or `gpt-4o-transcribe`, `whisper-1`) |
| Groq | `STT_PROVIDER=openai`, `STT_BASE_URL=https://api.groq.com/openai/v1`, `STT_MODEL=whisper-large-v3-turbo` |
| ElevenLabs Scribe | `STT_PROVIDER=elevenlabs`, `STT_MODEL=scribe_v2`, optional `STT_KEYTERMS=a,b,c` (billed extra by ElevenLabs) |
| Self-hosted | `STT_PROVIDER=openai`, `STT_BASE_URL=http://your-server/v1`, `STT_MODEL=...` (speaches, whisper.cpp server, LocalAI, vLLM) |

`STT_LANGUAGE` sets the default language (`auto` by default); each user can override it with `/lang`. `STT_PROMPT` adds a fixed hint for OpenAI-compatible providers, for example `Chat in Russian, Serbian and English. Names: Marko, Ana.`

## Commands

- `/start`, `/help`: how to use the bot.
- `/lang sr` | `/lang auto` | `/lang default`: your transcription language.
- `/stats` (admin): requests, errors and audio minutes per day/week/month/overall, plus top users.
- `/users` (admin): number of registered users.

## Access control

Transcription costs money, so the bot is private by default. Allowed users are the admin (`ADMIN_CHAT_ID`), the IDs in `ALLOWED_USER_IDS`, or everyone if `ALLOW_EVERYONE=true`. Anyone else is turned away, and the admin is told who tried. Only the person who sent a file can press its Cancel button.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `TELEGRAM_BOT_API_TOKEN` | required | Bot token |
| `BOT_API_URL` | `https://api.telegram.org` | Set by docker-compose to the local server |
| `ADMIN_CHAT_ID` | none | Admin user ID |
| `ALLOWED_USER_IDS` | empty | Comma-separated user IDs |
| `ALLOW_EVERYONE` | `false` | Open the bot to everyone |
| `STT_PROVIDER` | `openai` | `openai` (any compatible API) or `elevenlabs` |
| `STT_BASE_URL` | per provider | API base URL |
| `STT_API_KEY` | none | Provider key (optional for keyless local servers) |
| `STT_MODEL` | `gpt-transcribe` / `scribe_v2` | Model ID |
| `STT_LANGUAGE` | `auto` | Default ISO-639 language code |
| `STT_PROMPT` | none | Fixed prompt (OpenAI-compatible) |
| `STT_KEYTERMS` | none | ElevenLabs keyterms |
| `CHUNK_SECONDS` | `600` | Split longer audio into pieces of this length |
| `MAX_MEDIA_FILESIZE` | `2G` | Reject bigger files |
| `FFMPEG_TIMEOUT_SECONDS` | `300` | Per ffmpeg/ffprobe call |
| `STT_TIMEOUT_SECONDS` | `300` | Per transcription request |
| `BUSINESS_INCLUDE_OWN` | `false` | Also transcribe your own messages in business chats |
| `DATA_DIR` | `./data` | SQLite stats and temporary files |

## Development

```
just test     # unit tests; media tests generate real audio/video with ffmpeg
just lint     # rustfmt + clippy -D warnings
just run-local
```

## License

MIT. See [LICENSE](LICENSE).
