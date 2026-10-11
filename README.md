# Mumble Archive

A Mumble bot that records every voice channel as separate per-speaker tracks,
stores them as compact Opus, and a static web UI (Vue + Rsbuild) to browse,
play back and export them like a multitrack editor.

```
 Mumble server ──TLS──▶ mumble-archive (Rust bot) ──▶ data/public/  ◀── nginx ◀── browser (static UI)
                        records voice it receives     catalog.json            HTML/JS/CSS + wasm Opus
                        per-speaker segments          channels.json           decoder; reads JSON indexes,
                        silence removed, VBR Opus     speakers.json           fetches audio by HTTP Range,
                        SQLite (private)              index/<day>.json        mixes/exports client-side
                                                      audio/<month>/<day>.pack
```

The bot runs no web server. It continuously (re)generates static files; any static
host serves them together with the built UI.

## Features

**Recording**
- Passive: records exactly the voice the server delivers to the bot — speech in its
  own channel (`home_channel`), whispers/shouts targeting it, and channels an admin
  makes it listen to (e.g. via "Listen to channel" in a Mumble client). It never
  subscribes to anything itself. Handles servers with thousands of deeply nested
  channels and many concurrent speakers.
- Each user's voice is a separate track. Talk bursts are attributed to the speaker
  (registered id → certificate hash → name), the channel they were in, and the exact
  wall-clock time. Shout/whisper is flagged.
- Presence (who was in which channel when) and bot uptime are recorded too.
- Crash-safe: in-progress bursts are written as Ogg Opus and recovered on restart.

**Storage**
- Silence inside bursts is not stored: bursts are split into speech regions that keep
  their exact timestamps.
- Speech is re-encoded with libopus VBR (default 12 kbit/s, 120 ms frames) and is
  **never encoded above the source bitrate**. Sources already at or below the
  target keep their original packets.
- Segments are packed into one append-only file per day with a ~20-byte header per
  segment, so there is no per-file filesystem slack.
- Typical result: ~7× smaller than the incoming Mumble stream, about 4–5 MB per hour
  of speech.

**Web UI** (static: HTML/JS/CSS + wasm)
- Multitrack timeline grouped by channel with per-speaker lanes, waveforms, presence
  bands and an overview strip of the day's activity.
- Channel lanes can be collapsed into one merged "channel stream".
- Gapless playback from any point, reconstructing the conversation with Web Audio.
  Mute/solo/volume per track and per channel, master volume, skip-silence, follow,
  loop.
- With a selection, playback starts at the selection start (or in place if the
  playhead is inside it) and stops at its end.
- Built for large servers:
  - virtualized channel tree with search, "active only", focus into subtree,
    tri-state subtree filters and depth badges;
  - virtualized speaker list ranked by talk time;
  - virtualized track headers.
- Client-side export of any range:
  - **downmix** (mono), **multichannel** (one channel per speaker track),
    **per Mumble channel**, or **stems** (zip of aligned mono files + manifest);
  - formats: WAV, or Opus where the browser supports WebCodecs.

## Requirements

- Rust (stable) to build the bot; `protoc` on `PATH` (or `PROTOC=…`) for the Mumble protobufs.
- `ffmpeg` with libopus at runtime.
- Node.js 20+ to build the UI.
- A Mumble server (Opus voice). Place the bot where the voice you want recorded is
  delivered to it: its home channel, or channels a server admin adds as listeners for it.

## Build

```sh
cargo build --release                  # target/release/mumble-archive
cd web && npm ci && npm run build      # web/dist
```

## Run

```sh
cp config.example.toml config.toml     # edit server address etc.
./target/release/mumble-archive --config config.toml
```

The bot shows Mumble's *recording* indicator by default (`announce_recording`).
Make sure the people you record know about it and agree.

### Serve the UI (nginx)

Copy `web/dist` to `/srv/mumble-archive/web` and point `/data/` at the bot's public
dir (`data/public` by default). See [`deploy/nginx.conf`](deploy/nginx.conf).
Plain static serving is all that's needed; `.pack` files are read with HTTP Range
requests, which nginx supports out of the box. Protect the site with `auth_basic`
or your SSO if recordings are private.

The UI fetches data from `data/` relative to its URL. To host data elsewhere,
put a `config.json` next to `index.html`:

```json
{ "dataUrl": "https://cdn.example.org/mumble-archive/" }
```

(Cross-origin hosts must allow CORS for `GET` with the `Range` header.)

### Development

```sh
cd web && DATA_TARGET=http://127.0.0.1:8088 npm run dev   # proxies /data to a static server
```

## CLI

```sh
mumble-archive [--config config.toml]           # run the bot
mumble-archive republish                        # rebuild all static index files
mumble-archive export --from "2026-10-11 20:00" --to "2026-10-11 21:30" \
    --layout downmix|tracks|channels|stems --format wav|flac|opus|mp3 \
    [--channels 3,4] [--speakers 7] --out meeting.flac
```

The CLI export uses ffmpeg (FLAC/MP3 available) and reads the private database
directly.

## Mumble server notes

- When testing with many clients from one IP, Mumble's autoban
  (`autobanAttempts`) may block them. The bot itself reconnects with backoff.

## Data layout

```
data/
  archive.db            SQLite: speakers, channels, segments, presence, uptime (private)
  bot-cert.pem/.key     bot identity (private)
  tmp/
  public/               ← publish this directory
    catalog.json        stats + list of days
    channels.json       [id, mumbleId, parentId, name, position, removedAt]
    speakers.json       [id, name, userId, firstSeen, lastSeen]
    index/YYYY-MM-DD.json   segments [id, speaker, channel, startMs, durMs, target,
                            status, path, offset, size, peaks], presence, uptime
    audio/YYYY-MM/YYYY-MM-DD.pack
    raw/                in-progress bursts (Ogg Opus), removed after processing
```

A pack is a concatenation of records:

```
"MAR1"  u32le len
varint segId, startMs, speakerId, channelId, samples, preSkip, packetCount
packetCount × (varint len, Opus packet)
```

Each record is self-describing (speaker, channel and timestamp are inside it), so a
pack can be re-indexed without the database.

## Testing

`mumble-sim` is a test client that streams an Ogg Opus file into a server as a fake
speaker, or builds large channel trees:

```sh
mumble-sim --server 127.0.0.1:64738 --name Alice --channel Lobby --file speech.opus --repeat 3 --pause 2000
mumble-sim --name SuperUser --password <pw> --tree 2000 --depth 30   # rerun until it reports 'tree complete'
```

Options: `--delay ms` before speaking, `--drop N` to simulate losing every Nth packet,
`--create 1` to create `--channel` if missing. `cargo test` runs the unit tests.
