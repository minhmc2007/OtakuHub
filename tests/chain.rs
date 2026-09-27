//! The full chain against the real provider: search, poster, episode list, server, embed,
//! master playlist, media playlist, bytes. The failures here are attribute renames, not compile errors.

use std::collections::HashMap;
use std::sync::Arc;

use otakuhub::cache::Kind;
use otakuhub::source::hianime::HiAnime;
use otakuhub::source::{AnimeSource, Mode, Stream};
use otakuhub::{config::Config, media, state::AppState};

/// A short, stable title with a single episode, so the walk is quick and the
/// episode list has something in it.
const QUERY: &str = "one piece";

/// The whole chain has to reach a playable segment, so the deadline is generous.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// A temp state wired to a temp data directory, so the cache is real but disposable.
async fn state() -> AppState {
    let dir = std::env::temp_dir().join(format!("otakuhub-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::default();
    config.cache_dir = dir.join("cache");
    config.transcode_dir = dir.join("cache/transcode");
    config.data_dir = dir;
    AppState::build(config)
        .await
        .expect("a temp state must build")
}

fn source() -> HiAnime {
    HiAnime::new("https://hianime.at", vec!["ZokoAnime".to_string()])
        .expect("the source client must build")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_chain_reaches_a_real_segment() {
    tokio::time::timeout(TIMEOUT, async {
        let state = state().await;
        let src = source();
        let http = src.client().clone();

        // 1. search
        let results = src.search(QUERY).await.expect("search must answer");
        assert!(!results.is_empty(), "search returned nothing for {QUERY:?}");
        let anime = results[0].clone();
        assert!(!anime.id.is_empty(), "a result must carry a slug");
        println!("search: {} ({})", anime.title, anime.id);

        // 2. poster: a real image, not a cached error page
        let poster_url = anime
            .poster
            .clone()
            .expect("at least one result carries a poster");
        let bytes = crate_fetch(&state, Kind::Thumb, &poster_url).await;
        assert!(
            otakuhub::proxy::looks_like_an_image(&bytes),
            "the poster is not an image: first bytes {:02x?}",
            &bytes[..bytes.len().min(8)]
        );
        let size = otakuhub::proxy::image_dimensions(&bytes)
            .expect("a jpeg or png should report its size");
        assert!(size.0 > 20 && size.1 > 20, "poster is implausibly small: {size:?}");
        println!("poster: {}x{} from {}", size.0, size.1, poster_url);

        // 3. episode list
        let episodes = src.episodes(&anime.id).await.expect("episodes must answer");
        assert!(!episodes.is_empty(), "no episodes for {}", anime.id);
        println!("episodes: {} found, first is {}", episodes.len(), episodes[0].number);

        // 4. servers, embed, master playlist
        let episode = episodes[0].clone();
        let stream: Stream = src
            .stream(&anime, &episode, Mode::Sub)
            .await
            .expect("the stream chain must resolve");
        assert!(!stream.qualities.is_empty(), "no renditions in the master playlist");
        assert!(
            stream.referer.starts_with("http"),
            "the CDN needs a referer, got {:?}",
            stream.referer
        );
        println!(
            "stream: {} rendition(s), best {}, referer {}",
            stream.qualities.len(),
            stream.best().map(|q| q.label.as_str()).unwrap_or("?"),
            stream.referer
        );

        // 5. the media playlist, fetched with the referer the CDN demands
        let best = stream.best().expect("at least one rendition");
        let playlist = crate_playlist(&http, &best.url, &stream.referer)
            .await
            .expect("the media playlist must fetch");
        assert!(playlist.starts_with("#EXTM3U"), "not a playlist: {playlist:.80}");
        assert!(
            playlist.contains("seg_") || playlist.contains(".ts") || playlist.contains(".m4s"),
            "no segments listed: {playlist:.200}"
        );
        println!("playlist: {} bytes", playlist.len());

        // 6. a segment's actual bytes
        let segment = playlist
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .expect("the playlist lists at least one segment");
        let absolute = otakuhub::source::m3u8::absolutise(&best.url, segment);
        let body = crate_segment(&http, &absolute, &stream.referer)
            .await
            .expect("a segment must fetch");
        assert!(!body.is_empty(), "the segment came back empty");
        assert!(body.len() > 1024, "segment is implausibly small: {} bytes", body.len());
        println!("segment: {} bytes from {absolute}", body.len());
    })
    .await
    .expect("the chain must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_survives_junk_queries() {
    tokio::time::timeout(TIMEOUT, async {
        let src = source();
        // A blank query is answered without a request, not with a 404.
        assert!(src.search("   ").await.expect("a blank query must not error").is_empty());
        // Nonsense still has to come back as an error or an empty list, never a panic.
        for term in ["zzzzzzqqqqxxxx", "!!", "a b c", "鬼滅の刃"] {
            match src.search(term).await {
                Ok(list) => println!("{term:?} gave {} results", list.len()),
                Err(e) => println!("{term:?} gave an error: {}", e.public()),
            }
        }
    })
    .await
    .expect("junk queries must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_rewrites_a_playlist_onto_our_own_routes() {
    tokio::time::timeout(TIMEOUT, async {
        let state = state().await;
        let src = source();
        let http = src.client().clone();
        let results = src.search(QUERY).await.expect("search must answer");
        let anime = results[0].clone();
        let episodes = src.episodes(&anime.id).await.expect("episodes must answer");
        let stream = src
            .stream(&anime, &episodes[0], Mode::Sub)
            .await
            .expect("the stream chain must resolve");
        let best = stream.best().expect("a rendition");

        let body = otakuhub::proxy::rendition_playlist(&state, &best.url, &stream.referer)
            .await
            .expect("the playlist must come through the cache");
        let rewritten = otakuhub::proxy::rewrite_for_proxy(&body, &best.url, &stream.referer);

        assert!(rewritten.contains("/media/segment?u="), "segments are not proxied");
        assert!(rewritten.contains("r=https%3A%2F%2F"), "the referer is not carried");
        assert!(!rewritten.contains(best.url.as_str()), "an upstream URL leaked to the browser");

        // The referer the segments are sent must be the embed origin, not the CDN's own
        // host, or the CDN answers 403 for them.
        let encoded = format!("r={}", percent_encode(stream.referer.trim_end_matches('/')));
        assert!(
            rewritten.contains(&encoded),
            "segments are not being sent with the referer the playlist was fetched with"
        );
        println!(
            "rewritten playlist, first lines:\n{}",
            &rewritten[..rewritten.len().min(300)]
        );

        // And the segment has to come back with the right bytes when fetched the way the
        // proxy will fetch it.
        let first = rewritten
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("/media/segment?u="))
            .expect("a segment link in the rewritten playlist");
        let query = first.split_once('?').map(|(_, q)| q).expect("a query string");
        let pairs: HashMap<_, _> = query.split('&').filter_map(|p| p.split_once('=')).collect();
        let segment = otakuhub::util::url_decode(pairs.get("u").expect("the upstream url"));
        let body = crate_segment(&http, &segment, &stream.referer)
            .await
            .expect("a segment must fetch with the playlist's referer");
        assert!(body.len() > 1024, "segment was only {} bytes", body.len());
        println!("proxied segment: {} bytes", body.len());
    })
    .await
    .expect("the proxy rewrite must not hang");
}

/// Fetch through the app's own cache, so the caching path is exercised too.
async fn crate_fetch(state: &AppState, kind: Kind, url: &str) -> Vec<u8> {
    let first = otakuhub::proxy::fetch_cached(state, kind, url, None)
        .await
        .expect("the fetch must succeed");
    // A second call must be served from disk and be byte identical.
    let second = otakuhub::proxy::fetch_cached(state, kind, url, None)
        .await
        .expect("the cached read must succeed");
    assert_eq!(first, second, "the cache returned something different");
    first
}

async fn crate_playlist(http: &reqwest::Client, url: &str, referer: &str) -> Result<String, String> {
    let res = http
        .get(url)
        .header(reqwest::header::REFERER, referer)
        .header(reqwest::header::USER_AGENT, media::transcode::AGENT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    res.text().await.map_err(|e| e.to_string())
}

async fn crate_segment(http: &reqwest::Client, url: &str, referer: &str) -> Result<Vec<u8>, String> {
    let res = http
        .get(url)
        .header(reqwest::header::REFERER, referer)
        .header(reqwest::header::USER_AGENT, media::transcode::AGENT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}


/// Percent encode the same way the app does, so the assertion matches the real output.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The provider trait has to stay usable as a trait object, since that is how the app
/// holds it. This is the assertion that keeps the boxed futures honest.
#[tokio::test]
async fn the_source_is_usable_as_a_trait_object() {
    let src: Arc<dyn AnimeSource> = Arc::new(source());
    let list = src.search(QUERY).await.expect("a dyn source must answer");
    assert!(!list.is_empty());
    println!("through a trait object: {} results", list.len());
}
