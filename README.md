# Mumble Archive

A Mumble bot that records every voice channel as separate per-speaker tracks,
stores them as compact Opus, and a static web UI (Vue + Rsbuild) to browse,
play back and export them like a multitrack editor.

```
 Mumble server ──TLS──▶ mumble-archive (Rust bot) ──▶ data/public/  ◀── nginx ◀── browser (static UI)
                        records voice it receives     meta.json               HTML/JS/CSS + wasm Opus
                        per-speaker segments          days.log                decoder; reads the logs and
                        silence removed, VBR Opus     channels.<gen>.log      audio by HTTP Range,
                        SQLite (private)              index/<day>.log         mixes/exports client-side
                                                      audio/<month>/<day>.pack
```

The bot runs no web server. It publishes static files that only ever grow by
appending (index logs and audio packs); the UI reads just the new bytes with HTTP
Range requests, so neither the bot's writes nor a refresh grow with the age of the
archive. Any static host that supports Range serves them together with the built UI.

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
- Voice over encrypted UDP (OCB2-AES128, like Mumble clients) when it works, with
  automatic, lossless fallback to the TCP tunnel (e.g. firewalled UDP) and recovery.
  A short reorder buffer puts UDP packets back in order; lost packets become silence
  so the timeline stays aligned.
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

- Rust (stable) plus a C compiler and `cmake` to build the bot (libopus is compiled from
  source and linked in; `protoc` is vendored, override with `PROTOC=…`).
- No runtime dependencies: Opus decoding/encoding and FLAC export are built in.
- Node.js 20+ to build the UI.
- A Mumble server (Opus voice). Place the bot where the voice you want recorded is
  delivered to it: its home channel, or channels a server admin adds as listeners for it.

## Build

```sh
cargo build --release                  # target/release/mumble-archive
cd web && npm ci && npm run build      # web/dist
```

Linux release binaries are best built with [cross](https://github.com/cross-rs/cross) as
**static musl executables**: no runtime dependencies, any distribution, and TCMalloc
instead of musl's slower allocator. `Cross.toml` selects images whose compilers can
build libopus and TCMalloc:

```sh
cross build --release --target x86_64-unknown-linux-musl    # target/x86_64-unknown-linux-musl/release/
cross build --release --target aarch64-unknown-linux-musl
```

glibc targets (`*-unknown-linux-gnu`) build too, but the binary then needs glibc ≥ 2.39.
If you built with cross's default images before, run `cargo clean` once.

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
mumble-archive republish                        # rebuild all published logs from the database (stop the bot first)
mumble-archive export --from "2026-10-11 20:00" --to "2026-10-11 21:30" \
    --layout downmix|tracks|channels|stems --format wav|flac|opus \
    [--channels 3,4] [--speakers 7] --out meeting.flac
```

The CLI export adds FLAC and runs on the server, reading the private database
directly.

## Capacity

Measured with simulated speakers all talking at once (continuous speech, 16-core machine,
server and bot on one Docker network):

| concurrent speakers | voice packets/s | recorded | transcode backlog after load |
|---|---|---|---|
| 50  | ~2 500  | 100 %   | none |
| 150 | ~7 000  | 100 % (all 241 200 packets over UDP; bot ~40 MB RAM) | none |
| 300 | ~14 000 | 99.25 % (test machine saturated: 300 clients + server + bot) | cleared within seconds |

The voice path never touches disk or SQLite (a separate writer thread owns the files),
so bursts of load don't drop packets; the UDP socket gets an 8 MB receive buffer.
Transcoding uses libopus in-process (~110× real time per core to encode, ~1100× to
decode); with the default workers (half the cores) it keeps up with hundreds of
simultaneous speakers. Anything beyond is queued safely on disk and processed later.

## Mumble server notes

- On Linux, very busy servers benefit from a larger UDP socket buffer than the kernel
  default: `sysctl -w net.core.rmem_max=8388608` (the bot warns when it is limited).
- UDP voice uses the server's regular port (UDP 64738 by default). If UDP is blocked
  between bot and server, voice keeps flowing over TCP; set `mumble.udp = false` to
  skip UDP entirely. With `RUST_LOG=mumble_archive=debug` the bot logs which
  transport voice arrives on.
- When testing with many clients from one IP, Mumble's autoban
  (`autobanAttempts`) may block them. The bot itself reconnects with backoff.

## Data layout

```
data/
  archive.db            SQLite: speakers, channels, segments, presence, uptime (private)
  bot-cert.pem/.key     bot identity (private)
  tmp/
  public/               ← publish this directory
    meta.json           server info, format version, current log names (rarely written)
    days.log            days with data; per-day totals once a day is over
    channels.<gen>.log  live channel tree (compacted into a new generation when it grows)
    index/YYYY-MM-DD.log    one day: the speakers and channels it mentions, segments
                            (with waveform peaks), presence, bot sessions
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

The `.log` files are append-only (`src/publog.rs`):

```
"MAL1"  record*        record = varint len, type byte, fields
```

Fields are LEB128 varints (zigzag for signed), strings/blobs are length-prefixed.
The bot never rewrites a log: it appends records (later definitions of the same
speaker/channel replace earlier ones), and a reader keeps the length it has parsed
and asks for `Range: bytes=<length>-` next time (`416` = nothing new). Incomplete
trailing records are ignored by readers and truncated by the bot after a crash. Day
logs carry their own speaker and channel names, so there is no global speaker list
and removed channels never pile up in the live channel tree; the channel log is
rewritten into a new generation (announced by a `NEXT` record and `meta.json`) only
when edits have made it several times larger than the live tree. Serve `.log` files
without gzip (compression disables Range); they are compact binary already.

Caching: once a day is over, its log and pack only change if a late clip arrives,
which also publishes a new day summary. The UI therefore requests a finished
day's files as `<file>?v=<summary>`; serve requests with `v` with a long
`max-age, immutable` and everything else with `no-cache` (see
`deploy/nginx.conf`). Waveform peaks live only in the day logs; the database
drops them once a clip is published (`republish` reuses them from the existing
logs, or recomputes them from the audio).

## Testing

`mumble-sim` is a test client that streams an Ogg Opus file into a server as a fake
speaker, or builds large channel trees:

```sh
mumble-sim --server 127.0.0.1:64738 --name Alice --channel Lobby --file speech.opus --repeat 3 --pause 2000
mumble-sim --name SuperUser --password <pw> --tree 2000 --depth 30   # rerun until it reports 'tree complete'
```

Options: `--delay ms` before speaking, `--drop N` to simulate losing every Nth packet,
`--create 1` to create `--channel` if missing. `cargo test` runs the unit tests.
