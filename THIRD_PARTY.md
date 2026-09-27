# Third party

Everything here is someone else's work. OtakuHub itself is the code in `src/`, the
templates and `static/app.css` and `static/app.js`. This file covers what ships inside the
repository, not what is fetched at build time.

## Vendored browser libraries

Both files are committed so the app works with no network and no CDN. Each was downloaded
from npm and is byte identical to the release named here, verified with `cmp`.

### static/htmx.min.js

    htmx 2.0.4
    Copyright the htmx authors
    https://htmx.org
    https://github.com/bigskysoftware/htmx
    Licence: 0BSD (Zero Clause BSD)
    Licence text: https://github.com/bigskysoftware/htmx/blob/master/LICENSE
    sha256: e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447

### static/hls.min.js

    hls.js 1.5.17
    Copyright (c) 2017 Dailymotion (http://www.dailymotion.com)
    https://github.com/video-dev/hls.js
    Licence: Apache License 2.0
    Licence text: https://github.com/video-dev/hls.js/blob/master/LICENSE
    sha256: 484054e8cd03d3f6d1781fb7f402bdc318d8a4c527f933a95c624e27cc9a9470

Firefox and Chromium based desktop browsers have no native HLS support, so the app uses
hls.js there and falls back to the native player where one exists.

Neither file keeps its licence header, because both are distributed minified. Each licence
above was read from the upstream project.

## Rust dependencies

289 crates in `Cargo.lock`. Licences are read from each crate's own manifest in the cargo
registry cache, not from this file's memory, so re-run the check below after a
`cargo update`.

| Licence | Crates |
| --- | --- |
| MIT OR Apache-2.0 | 104 |
| no licence field in the manifest | 57 |
| MIT | 51 |
| Unicode-3.0 | 18 |
| Apache-2.0 OR MIT | 11 |
| MIT/Apache-2.0 | 11 |
| ISC | 5 |
| BSD-3-Clause | 4 |
| MPL-2.0 | 4 |
| Apache-2.0 | 3 |
| Apache-2.0 OR ISC OR MIT | 2 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 2 |
| Unlicense OR MIT | 2 |
| WTFPL | 2 |
| Zlib | 2 |
| (Apache-2.0 OR MIT) AND BSD-3-Clause | 1 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | 1 |
| 0BSD OR MIT OR Apache-2.0 | 1 |
| Apache-2.0 / MIT | 1 |
| Apache-2.0 AND ISC | 1 |
| Apache-2.0 OR BSL-1.0 | 1 |
| Apache-2.0/MIT | 1 |
| BSD-3-Clause AND MIT | 1 |
| BSD-3-Clause/MIT | 1 |
| MIT AND BSD-3-Clause | 1 |
| MIT OR Zlib OR Apache-2.0 | 1 |

Direct dependencies and their licences:

    axum 0.8.9                  MIT
    tokio 1.53.1                MIT
    tower 0.5.3                 MIT
    tower-http 0.7.1            MIT
    askama 0.16.1               MIT OR Apache-2.0
    reqwest 0.13.5              MIT OR Apache-2.0
    serde 1.0.229               MIT OR Apache-2.0
    serde_json 1.0.151          MIT OR Apache-2.0
    scraper 0.27.0              ISC
    rusqlite 0.40.2             MIT
    ffmpeg-next 9.0.0           WTFPL
    sysinfo 0.39.6              MIT
    sha2 0.10.9                 MIT OR Apache-2.0
    base64 0.22.1               MIT OR Apache-2.0
    percent-encoding 2.3.2      MIT OR Apache-2.0
    thiserror 2.0.21            MIT OR Apache-2.0
    tracing 0.1.44              MIT
    tracing-subscriber 0.3.23   MIT
    argon2 0.5.3                MIT OR Apache-2.0
    password-hash 0.5.0         MIT OR Apache-2.0
    getrandom 0.3.4             MIT OR Apache-2.0
    tempfile 3.27.0             MIT OR Apache-2.0

To regenerate this table:

    cargo metadata --format-version 1 \
      | python3 -c 'import json,sys,collections; d=json.load(sys.stdin); \
          g=collections.Counter(); [g.update([p["license"] or "none"]) \
          for p in d["packages"]]; [print(f"{k}: {v}") for k,v in sorted(g.items())]'

### The 57 with no licence field

Windows, wasm, macOS and Redox support crates, none of which build on Linux. They are
platform plumbing for targets this app does not ship to, so no licence text is bundled for
them. The ones a Linux build needs are all in the table above.

## Runtime, not bundled

These are linked or loaded from the system, so they are not in this repository. They still
have to be present for the app to work.

| Component | Licence |
| --- | --- |
| FFmpeg libraries (libavcodec, libavformat, libavutil, libavfilter, libswscale, libswresample) | LGPL-2.1 or later, or GPL-2.0 or later |
| SQLite | Public domain |
| hianime.at | no published licence |

`ffmpeg-next` is WTFPL but that only covers the Rust bindings. The libav libraries it
calls into are LGPL, and which of the LGPL or GPL terms applies depends on how your
distribution was built.

## ani-cli

The fetch chain, the embed deobfuscation and the quality ladder follow
[ani-cli](https://github.com/pystardust/ani-cli) by Anidlarem. No code is copied from it and
it is not a dependency in `Cargo.toml`, so it carries no licence obligation here.

## This project

    OtakuHub
    Licence: GPL-3.0-only
    See LICENSE
