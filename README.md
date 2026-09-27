# OtakuHub

```
cargo run --release
# listening on http://127.0.0.1:8080
```

No account exists on first start. Create one, then everything is per user.

## What it does

- Browse and search the provider, with titles, posters and episode lists.
- Play any episode directly, or pick a different server when one is down.
- Convert an episode to a quality the source does not offer, in the background. The result
  is a cached local HLS playlist.
- Playlists, per user, with a `Favourite` one created on signup.
- History, and a settings page showing CPU, GPU, RAM, cache size and disk.

## Install

Requires a Rust toolchain and the ffmpeg libraries. `ffmpeg-next` links the system
libav:

```sh
sudo pacman -S ffmpeg          # Arch
sudo apt install libavcodec-dev libavformat-dev libavutil-dev \
    libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev   # Debian
```

Then:

```sh
cargo build --release
./target/release/otakuhub
```

For Intel Quick Sync specifically you also need the runtime dispatcher, which is a
separate package on most distributions (`intel-opencl-icd`, `libmfx`, `vpl-gpu-rt`).
Without it the encoder probe rejects QSV with a reason in the startup log and moves on.

## Configuration

Every key has a default. `--bind`, `--port`, `--cache-max-bytes` and `--data-dir` are taken
from the flag or the env var when either is set, and only from `otakuhub.toml` when neither
is. The remaining keys read the file first and the env var after it.

```
-b, --bind <ADDR>          listen address          [default: 127.0.0.1]
-p, --port <PORT>          listen port             [default: 8080]
-d, --data-dir <PATH>      state, db and cache     [default: $XDG_DATA_HOME/otakuhub]
    --cache-max-bytes <N>  cache ceiling in bytes   [default: 8589934592]
    --software-encode      ignore hardware encoders and use libx264/libx265
```

| Variable | |
|---|---|
| `OTAKUHUB_BIND` `OTAKUHUB_PORT` `OTAKUHUB_DATA_DIR` | as the flags |
| `OTAKUHUB_CACHE_MAX_BYTES` | as the flag |
| `OTAKUHUB_HW_ENCODE` | `1` or `true` forces hardware on, any other value forces it off |
| `OTAKUHUB_LOG` | tracing filter, e.g. `otakuhub=debug` |

`<data-dir>/otakuhub.toml` is read if it exists. See `otakuhub.example.toml`; it also
covers the cache ceiling, segment TTL, transcode timeouts and the upstream base URL.

The startup banner prints the bind address, the data directory, the cache ceiling, the
hardware setting and which encoder was chosen.

## Where your data goes

Everything lives under the data directory, which defaults to
`$XDG_DATA_HOME/otakuhub` or `~/.local/share/otakuhub`:

```
otakuhub.sqlite        users, sessions, playlists, history, settings, job index
cache/                 proxied images and HLS segments
cache/transcode/       finished conversions, one directory per job
otakuhub.toml          optional, read at startup
```

The cache is trimmed once a minute down to `--cache-max-bytes`, least recently used
first. Segments older than `segment_ttl_secs` are re-fetched, since upstream links are
short lived. `Settings` shows the current total and has a purge button.

Deleting the SQLite file is a full reset: you will need to sign up again and the cache
becomes orphaned, so delete `cache/` too.

## Accounts

Signup takes a username and a password. The password is stored as an Argon2id hash.
Accounts created by an older build hold a SHA512 digest instead; they sign in once and are
rewritten to Argon2id on that sign in.

Sessions are random tokens in an HTTP only cookie, valid for 30 days. Changing a password
signs out every other session.

## Quality and conversion

Sources offer their own renditions and the player uses the closest one. When the highest
available is still below what you want, a conversion fills the gap: choose a height and
a codec on the watch page and the job runs in the background.

| Height | |
|---|---|
| default | Use the source as it is, no conversion |
| 360 / 480 / 720 / 1080 / 1440 / 2160 | Convert to that height |

Codec is per user, `h264` or `h265`, and applies to conversions only. A conversion writes
a local HLS playlist and the watch page switches to it when the job finishes, so you can
keep watching the source stream in the meantime.

Encoders are probed once at startup, in this order:

| Encoder | Requires |
|---|---|
| Quick Sync | `/dev/dri/renderD128`, `libmfx`, the vpl GPU runtime |
| NVENC | an NVIDIA card and a driver that exposes `hevc_nvenc` |
| VAAPI | `/dev/dri/renderD128` and a driver that exposes `h264_vaapi` |
| AMF | `libamf` present |
| Software | `libx264` / `libx265`, only when none of the above work |

Anything hardware-based is preferred. Software encoding is the last resort and is
reported as such in the log.

## Development

```sh
cargo test                      # unit tests
cargo test -- --test-threads=1  # plus the live integration tests
cargo clippy --all-targets
```

`tests/chain.rs` and `tests/transcode.rs` hit the real provider and the real libav
pipeline, so they need a network. Run them serially with `--test-threads=1`. They fail
when upstream changes shape.

## Layout

```
src/config.rs     flags, env vars and the TOML file
src/db/           one module per table
src/source/       the provider port: search, episodes, servers, the m3u8 deobfuscation
src/media/        probe, transcode pipeline, background jobs, byte ranges
src/proxy.rs      image and HLS proxying, the on-disk cache
src/web/          axum handlers, Askama view models, templates
templates/        the pages
static/app.css    the neumorphic stylesheet
static/app.js     hls.js wiring, the htmx bits, progress reporting
```

## Credits

`THIRD_PARTY.md` lists everything that ships in here: the vendored htmx and hls.js
builds with their licences and hashes, the licence breakdown of all 289 crates, and the
runtime libraries linked from the system. ani-cli is the reference for the provider
behaviour this app reproduces.

Licensed under GPL-3.0-only. See `LICENSE`.

