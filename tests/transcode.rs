//! The conversion path against a real stream: a full demux, scale, upload, encode and mux
//! cycle on the machine's real encoder, which the unit tests cannot cover.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use otakuhub::media::probe::{self, Hardware};
use otakuhub::media::{transcode, Codec};
use otakuhub::source::hianime::HiAnime;
use otakuhub::source::{AnimeSource, Mode};

/// Long enough for a handful of seconds of video, short enough to keep the test quick.
const SAMPLE_SECONDS: u64 = 6;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A short, stable title.
const QUERY: &str = "one piece";

fn source() -> HiAnime {
    HiAnime::new("https://hianime.at", vec!["ZokoAnime".to_string()])
        .expect("the source client must build")
}

/// The first playable stream of the first search result.
async fn first_stream() -> (String, String, String, String) {
    let src = source();
    let results = src.search(QUERY).await.expect("search must answer");
    let anime = results.first().expect("at least one result").clone();
    let episodes = src.episodes(&anime.id).await.expect("episodes must answer");
    let episode = episodes.first().expect("at least one episode").clone();
    let stream = src
        .stream(&anime, &episode, Mode::Sub)
        .await
        .expect("the stream chain must resolve");
    let best = stream.best().expect("at least one rendition").clone();
    (anime.id, episode.id, best.url, stream.referer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_detected_encoder_really_encodes_a_frame() {
    // The probe claims a working encoder. This is the claim being checked.
    let hardware: Hardware = probe::detect(true).expect("the probe must answer");
    println!(
        "detected: {} on {} ({})",
        hardware.encoder.label(),
        hardware.device,
        hardware.detail
    );
    for (name, why) in &hardware.rejected {
        println!("rejected: {name}: {why}");
    }

    let (w, h) = (256u32, 144u32);
    let mut enc = transcode::VideoEncoder::open(
        hardware.encoder,
        Codec::H264,
        w,
        h,
        ffmpeg::Rational::new(1, 24),
        &hardware.device,
    )
    .expect("the detected encoder must open");
    assert_eq!(enc.is_hardware(), hardware.is_hardware());

    let frame = transcode::synthetic_frame(w, h, enc.pixel_format());
    enc.send(&frame).expect("a frame must be accepted");
    enc.flush_eof().expect("the encoder must flush");
    let packets = enc.take_packets(8);
    assert!(!packets.is_empty(), "no packets came out of the encoder");
    // A packet with no timestamp cannot be muxed, so an encoder that produces one is
    // useless even though it "works".
    for p in &packets {
        assert!(p.dts().is_some(), "a packet came out with no dts");
    }
    println!("{} packets from one frame", packets.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transcode_produces_a_playable_playlist() {
    tokio::time::timeout(TIMEOUT, async {
        let (_anime, _episode, url, referer) = first_stream().await;
        println!("source: {url}");

        let hardware = probe::detect(true).expect("the probe must answer");
        let dir = std::env::temp_dir().join("otakuhub-transcode-test");
        let _ = std::fs::remove_dir_all(&dir);

        let request = transcode::Request {
            out_dir: dir.clone(),
            referer: Some(referer),
            input_url: url,
            backend: hardware.encoder,
            device: Some(std::path::PathBuf::from(&hardware.device)),
            limit_seconds: Some(SAMPLE_SECONDS),
            max_height: 480,
            codec: Codec::H264,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let result = {
            let cancel = Arc::clone(&cancel);
            let seen = Arc::clone(&seen);
            tokio::task::spawn_blocking(move || {
                let mut on_progress = |p: transcode::Progress| {
                    seen.lock().unwrap().push((p.frames, p.out_height));
                };
                transcode::run(&request, &cancel, &mut on_progress)
            })
            .await
            .expect("the worker must not panic")
        };
        result.expect("the transcode must succeed");

        let playlist = std::fs::read_to_string(dir.join("index.m3u8"))
            .expect("a playlist must have been written");
        assert!(playlist.starts_with("#EXTM3U"), "not a playlist: {playlist:.100}");
        assert!(playlist.contains("#EXT-X-ENDLIST"), "the playlist was never closed");

        let segments: Vec<&str> = playlist
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(!segments.is_empty(), "no segments were written");
        println!("{} segments in the playlist", segments.len());

        // Every segment the playlist names must exist on disk and have bytes in it.
        let mut total = 0u64;
        for name in &segments {
            let path = dir.join(name);
            let size = std::fs::metadata(&path)
                .unwrap_or_else(|e| panic!("{name} is listed but unreadable: {e}"))
                .len();
            assert!(size > 0, "{name} is empty");
            total += size;
        }
        println!("{total} bytes across {} segments", segments.len());

        // And libav has to be able to read back what we wrote.
        let probe = transcode::probe_size(&dir.join("index.m3u8").to_string_lossy(), None);
        let (w, h) = probe.expect("the output must be readable by libav");
        assert_eq!(h, 480, "the output is not the height that was asked for");
        assert!(w > 0 && w % 2 == 0, "width {w} is not a usable size");
        println!("output: {w}x{h}");

        // The progress callback has to have reported the output height at least once.
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|(_, h)| *h == 480),
            "progress never reported the output height: {seen:?}"
        );
    })
    .await
    .expect("the transcode must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_transcode_stops_promptly() {
    tokio::time::timeout(TIMEOUT, async {
        let (_anime, _episode, url, referer) = first_stream().await;
        let hardware = probe::detect(true).expect("the probe must answer");
        let dir = std::env::temp_dir().join("otakuhub-cancel-test");
        let _ = std::fs::remove_dir_all(&dir);

        let request = transcode::Request {
            input_url: url,
            out_dir: dir.clone(),
            referer: Some(referer),
            backend: hardware.encoder,
            device: Some(std::path::PathBuf::from(&hardware.device)),
            limit_seconds: None,
            max_height: 360,
            codec: Codec::H264,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        tokio::spawn(async move {
            // Cancel while the run is still going, which the interrupt callback should
            // notice within a network round trip.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            flag.store(true, Ordering::Relaxed);
        });

        let result = tokio::task::spawn_blocking(move || {
            let mut on_progress = |_: transcode::Progress| {};
            transcode::run(&request, &cancel, &mut on_progress)
        })
        .await
        .expect("the worker must not panic");

        assert!(result.is_err(), "a cancelled run must not report success");
        println!("cancelled as expected: {}", result.unwrap_err());
    })
    .await
    .expect("the cancel must not hang");
}
