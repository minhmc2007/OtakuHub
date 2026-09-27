//! The caching proxy. One place that turns an upstream URL into a local response, for
//! images, playlists, segments, keys and subtitles alike.

use std::time::Duration;

use crate::cache::{self, Kind};
use crate::error::AppResult;
use crate::state::AppState;
use crate::util;

const AGENT: &str = crate::media::transcode::AGENT;

/// How long a playlist is held before it is refetched. Playlists are cheap and go stale
/// quickly, so this is short. Segments and images are cached until evicted.
const PLAYLIST_TTL: i64 = 60;

/// Generous next to a real segment or poster, but bounded so a bad upstream cannot
/// exhaust memory.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Refuse a URL whose host is a literal private address or one of the reserved local names.
/// The host is never resolved, so a name that points at a private address still passes.
pub fn assert_public_url(url: &str) -> crate::error::AppResult<()> {
    use std::net::IpAddr;

    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| {
            crate::error::AppError::bad("only http and https urls can be fetched")
        })?;
    // The authority runs to the first slash, question mark or fragment.
    let authority = match rest.split(['/', '?', '#']).next().unwrap_or("").rsplit_once('@') {
        Some((_, host)) => host,
        None => rest.split(['/', '?', '#']).next().unwrap_or(""),
    };
    // Strip the port, taking care not to eat the colons in a bare IPv6 literal.
    let host = match authority.rfind(']') {
        Some(close) => &authority[..=close],
        None => authority.split(':').next().unwrap_or(""),
    };
    if host.is_empty() {
        return Err(crate::error::AppError::bad("no host in that url"));
    }

    // A string check, not a resolve: a name that points at 127.0.0.1 passes.
    let lower = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "metadata.google.internal"
    {
        return Err(crate::error::AppError::bad("that host is not reachable"));
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        if is_private(ip) {
            return Err(crate::error::AppError::bad("that address is not public"));
        }
    }
    Ok(())
}

/// Loopback, private, link local, broadcast and the unspecified address.
fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                // 169.254.0.0/16, which is where a cloud metadata service lives.
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
                // 100.64.0.0/10, carrier grade NAT.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique local fc00::/7 and link local fe80::/10.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4 mapped, which would otherwise smuggle a v4 address past the v4 check.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(std::net::IpAddr::V4(v4)))
        }
    }
}

/// Fetch bytes with the provider's required headers, caching by URL.
pub async fn fetch_cached(
    state: &AppState,
    kind: Kind,
    url: &str,
    referer: Option<&str>,
) -> AppResult<Vec<u8>> {
    // Before the cache, since a hit never reaches `fetch_remote` where the check also lives.
    assert_public_url(url)?;
    let path = state.cache().path_of(kind, url);
    if let Some(body) = state.cache().read(&path) {
        state.cache().touch(state.db(), kind, url)?;
        cache::note_served(body.len());
        return Ok(body);
    }
    let body = fetch_remote(state, url, referer).await?;
    state.cache().store(&path, &body)?;
    state.cache().track(state.db(), kind, url, &path, body.len() as u64)?;
    Ok(body)
}

/// Fetch an image, with a size floor check so a 1x1 tracking pixel or an error page
/// never gets cached as a poster.
pub async fn fetch_image(state: &AppState, url: &str) -> AppResult<Vec<u8>> {
    let body = fetch_cached(state, Kind::Thumb, url, None).await?;
    if !looks_like_an_image(&body) {
        return Err(crate::error::AppError::upstream(
            "image",
            format!("{} did not return a picture", crate::source::m3u8::file_hint(url)),
        ));
    }
    Ok(body)
}

/// The type is read from the bytes, not the URL, since the same host serves jpeg, webp and
/// avif and the response goes out with `nosniff` set.
pub fn image_type(body: &[u8]) -> Option<&'static str> {
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff];
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const BMP: &[u8] = b"BM";
    const ICO: &[u8] = &[0x00, 0x00, 0x01, 0x00];

    if body.starts_with(JPEG) {
        return Some("image/jpeg");
    }
    if body.starts_with(PNG) {
        return Some("image/png");
    }
    if body.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    // RIFF is a container: webp is the only image flavour that matters here, and the
    // form type sits at offset 8.
    if body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // ISO base media, which is how avif and heic are identified.
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        let brand = &body[8..12];
        return match brand {
            b"avif" | b"avis" => Some("image/avif"),
            b"heic" | b"heix" | b"mif1" => Some("image/heic"),
            _ => Some("image/avif"),
        };
    }
    if body.starts_with(BMP) {
        return Some("image/bmp");
    }
    if body.starts_with(ICO) {
        return Some("image/x-icon");
    }
    None
}

/// True when the bytes are a picture. Used to keep an HTML error page out of the cache.
pub fn looks_like_an_image(body: &[u8]) -> bool {
    image_type(body).is_some()
}

/// Read the pixel dimensions out of a JPEG or PNG header, so the UI can reserve space
/// before the image arrives. Returns `None` for anything else.
pub fn image_dimensions(body: &[u8]) -> Option<(u32, u32)> {
    if body.starts_with(&[0xff, 0xd8, 0xff]) {
        return jpeg_size(body);
    }
    if body.starts_with(&[0x89, b'P', b'N', b'G']) && body.len() >= 24 {
        let w = u32::from_be_bytes(body[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(body[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    webp_size(body)
}

/// `VP8X` stores the canvas size as three bytes each, one less than the real value. A plain
/// lossy `VP8 ` file puts a 14 bit width and height after the start code instead.
fn webp_size(body: &[u8]) -> Option<(u32, u32)> {
    match body.get(12..16)? {
        b"VP8X" if body.len() >= 30 => {
            let read = |at: usize| u32::from(body[at]) | u32::from(body[at + 1]) << 8 | u32::from(body[at + 2]) << 16;
            let (w, h) = (read(24) + 1, read(27) + 1);
            (w > 0 && h > 0).then_some((w, h))
        }
        b"VP8 " if body.len() >= 30 => {
            // The frame tag is three bytes, then a three byte start code, then the size.
            if body[23..26] != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let w = (u32::from(body[26]) | u32::from(body[27]) << 8) & 0x3fff;
            let h = (u32::from(body[28]) | u32::from(body[29]) << 8) & 0x3fff;
            (w > 0 && h > 0).then_some((w, h))
        }
        _ => None,
    }
}

/// Walk JPEG segments until a start of frame marker, which carries the real size.
fn jpeg_size(body: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < body.len() {
        if body[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = body[i + 1];
        // SOF0 through SOF15, skipping the non frame markers in that range.
        let is_sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            let h = u16::from_be_bytes([body[i + 5], body[i + 6]]) as u32;
            let w = u16::from_be_bytes([body[i + 7], body[i + 8]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        let len = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Fetch straight from upstream, bypassing the cache. Used for playlists that must be
/// current, and as the miss path of `fetch_cached`.
pub async fn fetch_remote(
    state: &AppState,
    url: &str,
    referer: Option<&str>,
) -> AppResult<Vec<u8>> {
    assert_public_url(url)?;
    let mut req = state.http().get(url).header(reqwest::header::USER_AGENT, AGENT);
    if let Some(r) = referer {
        req = req.header(reqwest::header::REFERER, r);
    }
    let res = req
        .send()
        .await
        .map_err(|e| crate::error::AppError::upstream("stream", format!("{url}: {e}")))?;
    let status = res.status();
    if !status.is_success() {
        return Err(crate::error::AppError::upstream(
            "stream",
            format!("HTTP {} from {}", status.as_u16(), crate::source::m3u8::file_hint(url)),
        ));
    }
    // Refuse an oversized body on the declared length, so the connection is dropped
    // before the whole thing is buffered.
    if let Some(len) = res.content_length() {
        if len as usize > MAX_BODY_BYTES {
            return Err(crate::error::AppError::upstream(
                "stream",
                format!("{} is larger than the fetch limit", crate::source::m3u8::file_hint(url)),
            ));
        }
    }
    let mut body = res
        .bytes()
        .await
        .map_err(|e| crate::error::AppError::upstream("stream", e.to_string()))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(crate::error::AppError::upstream(
            "stream",
            "that response is larger than the fetch limit".to_string(),
        ));
    }
    Ok(std::mem::take(&mut body).to_vec())
}

/// A running transcode is polled without caching, because its playlist grows. A direct source
/// playlist is cached briefly, so a segment request does not add a round trip.
pub async fn rendition_playlist(
    state: &AppState,
    quality_url: &str,
    referer: &str,
) -> AppResult<String> {
    // A cache hit never reaches the fetch, so check here too.
    assert_public_url(quality_url)?;
    if let Some(body) = state.cache().read(&state.cache().path_of(Kind::Segment, quality_url)) {
        let age = now_minus_touch(state, quality_url);
        if age < PLAYLIST_TTL {
            let text = String::from_utf8_lossy(&body).into_owned();
            state.cache().touch(state.db(), Kind::Segment, quality_url)?;
            return Ok(text);
        }
    }
    let body = fetch_remote(state, quality_url, Some(referer)).await?;
    let path = state.cache().path_of(Kind::Segment, quality_url);
    state.cache().store(&path, &body)?;
    state
        .cache()
        .track(state.db(), Kind::Segment, quality_url, &path, body.len() as u64)?;
    String::from_utf8(body)
        .map_err(|_| crate::error::AppError::upstream("stream", "the playlist was not text"))
}

/// Seconds since a cached playlist was last used.
fn now_minus_touch(state: &AppState, url: &str) -> i64 {
    state
        .db()
        .with(|c| {
            Ok(c.query_row(
                "SELECT touched_at FROM media_cache WHERE key = ?1",
                [cache::key_for(Kind::Segment, url)],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0))
        })
        .map(|touched| crate::time::now_secs() - touched)
        .unwrap_or(i64::MAX)
}

/// `referer` has to be the origin the playlist itself was fetched with, the embed page and
/// not the CDN. The hosts check that separately, and most segments 403 on the wrong one.
pub fn rewrite_for_proxy(body: &str, playlist_url: &str, referer: &str) -> String {
    let origin = referer.to_string();
    crate::source::m3u8::rewrite_media(body, playlist_url, &move |upstream| {
        format!(
            "/media/segment?u={}&r={}",
            util::url_encode(upstream),
            util::url_encode(&origin)
        )
    })
}

/// Same rewrite for a local job playlist, whose segments need no upstream fetch.
pub fn rewrite_for_local(body: &str, job_id: &str) -> String {
    crate::source::m3u8::rewrite_media(body, "", &move |name| {
        format!("/media/job/{}/{}", job_id, name.rsplit('/').next().unwrap_or("seg"))
    })
}

/// Where a proxied segment should be looked for on disk, and what kind it is.
pub fn classify(url: &str) -> Kind {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".vtt") || path.ends_with(".m3u8") && path.contains("subs") {
        Kind::Subtitle
    } else {
        Kind::Segment
    }
}

/// Fetch one proxied segment, through the cache.
pub async fn segment(state: &AppState, url: &str, referer: &str) -> AppResult<Vec<u8>> {
    // A cache hit never reaches the fetch, so check here too.
    assert_public_url(url)?;
    let path = state.cache().path_of(classify(url), url);
    if let Some(body) = state.cache().read(&path) {
        state.cache().touch(state.db(), classify(url), url)?;
        cache::note_served(body.len());
        return Ok(body);
    }
    let body = fetch_remote(state, url, Some(referer)).await?;
    // Written under the URL hash, which is the hash the playlist rewrite points at, so a repeat is a disk read.
    state.cache().store(&path, &body)?;
    state.cache().track(state.db(), classify(url), url, &path, body.len() as u64)?;
    cache::note_served(body.len());
    Ok(body)
}

/// Background upkeep: trim to the ceiling, drop expired entries, remove job directories
/// whose row is gone. Cheap enough to run on a timer.
pub async fn maintain(state: &AppState) {
    let ttl = state.config().segment_ttl_secs;
    let max = state.config().cache_max_bytes;
    let (db, cache) = (state.db().clone(), state.cache().clone());
    let upkeep = tokio::task::spawn_blocking(move || {
        let expired = cache.sweep_expired(&db, ttl);
        let evicted = if expired.is_ok() { cache.evict_to(&db, max) } else { Ok(0) };
        (expired, evicted)
    });
    let result = upkeep.await;
    match result {
        Ok((expired, evicted)) => match (expired, evicted) {
            (Ok(expired), Ok(evicted)) if expired + evicted > 0 => {
                tracing::info!(expired, evicted, "cache trimmed");
            }
            (Err(e), _) | (_, Err(e)) => tracing::warn!(error = %e, "cache upkeep failed"),
            _ => {}
        },
        Err(e) => tracing::warn!(error = %e, "cache upkeep task failed"),
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(AGENT)
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("a client with only timeouts set always builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    // SOI, then a start of frame marker: length, precision, height, width, components.
    const JPEG: &[u8] = &[
        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x01, 0xe0, 0x00, 0x64, 0x03, 0x01, 0x22,
        0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
    ];

    #[test]
    fn image_magic_is_recognised() {
        assert_eq!(image_type(JPEG), Some("image/jpeg"));
        assert_eq!(
            image_type(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("image/png")
        );
        assert_eq!(image_type(b"GIF89a...."), Some("image/gif"));
        assert!(looks_like_an_image(JPEG));
        assert!(!looks_like_an_image(b"<html>404</html>"));
        assert!(!looks_like_an_image(b""));
    }

    #[test]
    fn webp_and_avif_are_told_apart_from_jpeg() {
    // Read from the bytes, since the same host serves webp and jpeg and the label goes out under nosniff.
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0x2a, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(image_type(&webp), Some("image/webp"));

        let mut avif = b"\x00\x00\x00\x20".to_vec();
        avif.extend_from_slice(b"ftypavif");
        assert_eq!(image_type(&avif), Some("image/avif"));

        // A RIFF that is not webp is not an image we can serve as one.
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0x2a, 0, 0, 0]);
        wav.extend_from_slice(b"WAVEfmt ");
        assert_eq!(image_type(&wav), None);
    }

    #[test]
    fn a_short_riff_is_not_webp() {
        assert_eq!(image_type(b"RIFF"), None);
        assert_eq!(image_type(b"RIFF\x2a\x00\x00\x00WE"), None);
    }

    /// A webp header of 30 bytes with the given chunk type and payload.
    fn webp_bytes(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&[0x2a, 0, 0, 0]);
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(chunk);
        out.extend_from_slice(&[10, 0, 0, 0]);
        out.extend_from_slice(payload);
        out.resize(30, 0);
        out
    }

    #[test]
    fn webp_dimensions_come_from_the_vp8x_chunk() {
        // One flag byte, three of padding, then the size as three bytes each, holding
        // one less than the real value.
        let webp = webp_bytes(b"VP8X", &[0x10, 0, 0, 0, 0x2b, 0x01, 0, 0xc7, 0x00, 0x00]);
        assert_eq!(image_dimensions(&webp), Some((300, 200)));
    }

    #[test]
    fn lossy_webp_dimensions_come_from_after_the_start_code() {
        // A plain VP8 file: frame tag, start code, then 14 bits of width and height.
        let mut payload = vec![0x30, 0x01, 0x00, 0x9d, 0x01, 0x2a];
        payload.extend_from_slice(&(600u16 & 0x3fff).to_le_bytes());
        payload.extend_from_slice(&(400u16 & 0x3fff).to_le_bytes());
        let webp = webp_bytes(b"VP8 ", &payload);
        assert_eq!(image_dimensions(&webp), Some((600, 400)));
    }

    #[test]
    fn jpeg_dimensions_are_read_from_the_frame_header() {
        assert_eq!(image_dimensions(JPEG), Some((100, 480)));
    }

    #[test]
    fn png_dimensions_are_read_from_the_ihdr_chunk() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&300u32.to_be_bytes());
        png.extend_from_slice(&200u32.to_be_bytes());
        assert_eq!(image_dimensions(&png), Some((300, 200)));
    }

    #[test]
    fn unknown_formats_have_no_size() {
        assert_eq!(image_dimensions(b"GIF89a"), None);
        assert_eq!(image_dimensions(&[0xff, 0xd8, 0xff, 0x00]), None);
    }

    #[test]
    fn proxy_rewrite_points_everything_at_the_local_route() {
        let body = "#EXTM3U\n#EXTINF:4,\nseg_1.ts\n#EXTINF:4,\nseg_2.ts\n";
        let out = rewrite_for_proxy(body, "https://cdn.test/v/1080/index.m3u8", "https://embed.test/");
        assert!(out.contains("/media/segment?u=https%3A%2F%2Fcdn.test%2Fv%2F1080%2Fseg_1.ts"));
        assert!(!out.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn segments_keep_the_referer_the_playlist_was_fetched_with() {
        // Not the CDN's own host, which gets a 403 on most segments.
        let body = "#EXTM3U\n#EXTINF:4,\nseg_1.ts\n";
        let out = rewrite_for_proxy(body, "https://cdn.test/v/1080/index.m3u8", "https://embed.test/");
        assert!(out.contains("r=https%3A%2F%2Fembed.test%2F"), "{out}");
        assert!(!out.contains("r=https%3A%2F%2Fcdn.test"), "{out}");
    }

    #[test]
    fn local_rewrite_uses_the_job_route() {
        let body = "#EXTM3U\n#EXTINF:4,\nseg_00001.ts\n";
        let out = rewrite_for_local(body, "abc123");
        assert!(out.contains("/media/job/abc123/seg_00001.ts"));
    }

    #[test]
    fn subtitle_urls_are_classified_separately() {
        assert_eq!(classify("https://h.test/subs/en.vtt"), Kind::Subtitle);
        assert_eq!(classify("https://h.test/v/1080/seg_1.ts"), Kind::Segment);
        assert_eq!(classify("https://h.test/s.ts?token=1"), Kind::Segment);
    }
}
