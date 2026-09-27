# AGENTS.md

## Commands

```sh
cargo build
cargo test -- --test-threads=1    # 219 tests, see below for why serial
cargo clippy --all-targets
```

Run the app:

```sh
cargo run -- --port 8080
OTAKUHUB_DATA_DIR=/tmp/oh OTAKUHUB_PORT=8099 cargo run
```

Single test: `cargo test --lib db::progress` or `cargo test --test routes the_url_check`.
Filter by substring, no exact-match flag needed.

## Tests

`--test-threads=1` is required, not stylistic. `tests/chain.rs` names one temp dir per
process and deletes it on entry, so a parallel run wipes the cache out from under a
sibling test.

Three of the four suites hit the live provider. They are slow, and they fail when hianime
changes shape or the CDN refuses a request.

| Suite | Needs network | What it covers |
| --- | --- | --- |
| `--lib` | no | 205 unit tests, run in under a second |
| `--test routes` | no | the router through `tower::ServiceExt::oneshot`, no socket |
| `--test chain` | yes | search, poster, episodes, servers, embed, playlist, segment |
| `--test transcode` | yes | the real libav pipeline on a real GPU |

`tests/routes.rs` is where a missing check between a handler and the browser gets caught.
A new security property or route belongs there, not in a unit test.

## Schema

`SCHEMA_VERSION` is 3. A new table or column needs two edits in `src/db/mod.rs`: the
`SCHEMA` string for fresh databases, and an `UPGRADES` entry for existing ones. Miss the
second and the table exists only on a new install. Miss the first and `Db::open_memory`
tests fail on a clean database.

## Static files

`web::static_dir` resolves in this order: `OTAKUHUB_STATIC`, then
`<data_dir>/static`, then a `static` dir beside the binary, then `./static`.

After editing anything in `static/`, copy it to the running server's dir, or it silently
serves the old copy:

```sh
cp static/app.css static/app.js "$OTAKUHUB_DATA_DIR/static/"
```

## Transcoding

`VideoEncoder::pixel_format` decides what the scaler outputs, and the scaler takes that
format as an argument. Do not hardcode NV12 in `Scaler::run`. A software encoder opened as
YUV420P and fed NV12 dies on frame 0 with `AVERROR_EXTERNAL`.

`Scaler::run` and `VideoEncoder::send` both call `carry_time`. `sws_scale_frame` and
`av_hwframe_transfer_data` move pixels and nothing else, so a frame that loses its pts
produces packets the muxer refuses with EINVAL and the conversion yields one unreadable
segment. Removing either call is a silent breakage.

`probe::verify` opens the encoder, sends a frame and reads a packet back, so the startup
probe matches the real pipeline. It rejects a packet with no dts and one with an absurd
one, which is how a broken backend gets skipped instead of discovered 40 minutes into an
episode. Keep the synthetic frame's format and timestamp in step with what the encoder
actually wants, or the probe fails a working encoder.

Hardware candidates run in the order given by `probe::candidates`: CUDA, QSV, AMF, VAAPI.
Each needs a device check, a codec check, then `verify`. If a hardware run produces nothing
usable at convert time, `media/jobs.rs` retries in software and rewrites the job row so
the UI does not claim a GPU encode that never happened.

## Ownership and ids

`jobs::key_id` hashes `user_id` with the request. It must, because two accounts asking
for the same episode collide on one row, and since the upsert leaves the owner alone, the
second account gets a job it can never poll.

A job id is a 24 character hex digest. `web::owned_job` checks that shape and then
requires an owned row before any path is built. Both halves matter: the shape is what
keeps `../..` out, the row is what keeps another account out.

## URLs

`proxy::assert_public_url` is a string check, not a resolve. It rejects literal private
addresses and five reserved names. A hostname that resolves to 127.0.0.1 passes it. Do not
read it as DNS rebinding protection.

It runs before the cache read as well as before the fetch. A cache hit never reaches the
fetch, and an entry cached while the check was missing stays on disk.

## Watch progress

Two tables, and the split matters. `history` is one row per series, so it only knows the
last episode opened. `episode_progress` is one row per episode, so it knows which of a
thousand are watched and where inside each one the viewer stopped.

The player reports to `POST /api/history/position` with `anime, ep_id, ep, t, d`. The
per episode row is the authority on resume positions. A position within 90 seconds of the
end marks the episode finished and stores zero, so a return visit does not drop the viewer
into the credits.

## HTML and JSON escaping

Askama escapes by default. `views.rs` builds HTML in Rust and the templates mark it
`| safe`, so the guarantee rests on the values already being escaped. Card attributes go
through `CardView::html` on purpose. `source::json_obj` escapes `hx-vals` as JSON, because
a title with a quote breaks htmx's parse and the button does nothing with no error.

## CSS

There is no CSS custom property shadow engine. Shadows are fixed pairs of
`--shadow-flat`, `--shadow-sm`, `--shadow-lg` and the inset variants, lit from the top
left. There is no draggable light orb. The light position was removed, and nothing should
reintroduce one.

`static/app.css` line 1 `@import`s Google Fonts, so a page render reaches the network for
typefaces. That matters for an app meant to work offline.

## Comments

One or two lines. Never a third. The owner has asked for this repeatedly.

No dash characters in prose. No `// --- section` banners, and no `/* ==== */` blocks. No
vocabulary from the no-ai-slop list, no dangling participle endings, no negative
parallelisms, and no comment that restates the line below it.

Good:

```rust
// The CDN rejects segments fetched with its own host as the referer.
// Without the pts the muxer refuses every packet, so the conversion fails.
```

## Docs

`README.md` is reference material. Commands, config, data layout, encoder table. No
pitch, no claims about what the project is good at.

`THIRD_PARTY.md` lists the vendored htmx and hls.js with sha256 sums, and a licence
breakdown of all 289 crates read from their own manifests. The hashes were checked with
`cmp` against the upstream npm releases. Regenerate the crate table after `cargo update`.
