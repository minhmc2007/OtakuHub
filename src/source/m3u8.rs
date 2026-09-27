//! HLS playlist handling: read a multivariant playlist, and rewrite one so every
//! URI points back at this server (which proxies and caches).

use crate::error::{AppError, AppResult};
use crate::source::Quality;

const STREAM_INF: &str = "#EXT-X-STREAM-INF:";
const MEDIA_SEQUENCE: &str = "#EXT-X-MEDIA-SEQUENCE:";
const TARGET_DURATION: &str = "#EXT-X-TARGETDURATION:";
const MEDIA: &str = "#EXT-X-MEDIA:";
const ENDLIST: &str = "#EXT-X-ENDLIST";

/// Handles both `RESOLUTION=1920x1080` and the older `NAME="1080p"` convention, since
/// providers mix them.
pub fn parse_master(body: &str, playlist_url: &str) -> AppResult<Vec<Quality>> {
    let mut out: Vec<Quality> = Vec::new();
    let mut pending: Option<(String, u64)> = None;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(STREAM_INF) {
            let bandwidth = attr(rest, "BANDWIDTH")
                .and_then(|v| v.parse().ok())
                .or_else(|| {
                    attr(rest, "AVERAGE-BANDWIDTH")
                        .and_then(|v| v.parse().ok())
                })
                .unwrap_or(0);
            pending = Some((rest.to_string(), bandwidth));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some((rest, bandwidth)) = pending.take() else {
            continue;
        };
        let (label, height) = label_and_height(&rest);
        out.push(Quality {
            label,
            height,
            bandwidth,
            url: absolutise(playlist_url, line),
        });
    }

    if out.is_empty() {
        return Err(AppError::upstream("playlist", "no renditions in the master playlist"));
    }
    // Highest first, ties broken by bandwidth so the label alone is not load bearing.
    out.sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    Ok(out)
}

/// `RESOLUTION` when present, otherwise digits from `NAME` or the URI.
fn label_and_height(rest: &str) -> (String, u32) {
    if let Some(res) = attr(rest, "RESOLUTION") {
        if let Some((w, h)) = res.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                return (format!("{h}p"), h.max(1).min(w.max(1)));
            }
        }
    }
    let name = attr(rest, "NAME").unwrap_or_default();
    let from_name = digits(&name);
    if from_name > 0 {
        return (name, from_name);
    }
    let uri = attr(rest, "URI").unwrap_or_default();
    let from_uri = digits(&uri);
    (from_uri.to_string(), from_uri)
}

/// First run of digits, so `1080p` and `index-1080` both give 1080.
fn digits(s: &str) -> u32 {
    let mut n = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            n.push(c);
        } else if !n.is_empty() {
            break;
        }
    }
    n.parse().unwrap_or(0)
}

/// Read one comma separated attribute out of an `EXT-X-STREAM-INF` line.
fn attr(line: &str, key: &str) -> Option<String> {
    for part in split_quoted(line, ',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{key}=")) {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}

/// Split on a separator, but not inside a quoted value. Bandwidth and CODECS lists
/// both contain commas inside quotes often enough to matter.
fn split_quoted(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c == sep && !in_quotes => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

pub fn absolutise(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    if let Some(rest) = target.strip_prefix("//") {
        let scheme = if base.starts_with("https") { "https" } else { "http" };
        return format!("{scheme}://{rest}");
    }
    if target.starts_with('/') {
        return origin(base).map(|o| format!("{o}{target}")).unwrap_or_else(|| target.to_string());
    }
    match base.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{target}"),
        None => target.to_string(),
    }
}

pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(format!("{scheme}://{host}"))
}

/// Last path segment of a URL, used as a cache file name hint.
pub fn file_hint(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("index");
    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if name.is_empty() { "index".into() } else { name }
}

/// Rewrite a media playlist. Comments and the target duration pass through untouched so
/// hls.js keeps its buffering maths correct.
pub fn rewrite_media(
    body: &str,
    playlist_url: &str,
    resolve: &dyn Fn(&str) -> String,
) -> String {
    let mut out = String::with_capacity(body.len() + 512);
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            // The key URI is rewritten in place: the browser fetches the key through the
            // proxy or it cannot decrypt the segments.
            if let Some(uri) = attr(rest, "URI") {
                let absolute = absolutise(playlist_url, &uri);
                let rewritten = format!("#EXT-X-KEY:{}", rest.replacen(&uri, &resolve(&absolute), 1));
                out.push_str(&rewritten);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if line.starts_with('#') {
            // Kept on purpose: without it a VOD playlist reads as live and never settles.
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let absolute = absolutise(playlist_url, line);
        out.push_str(&resolve(&absolute));
        out.push('\n');
    }
    out
}

/// Present on a playlist whose last segment is the last segment. A transcode that is
/// still running never writes it, which is how the player knows to keep polling.
pub fn is_complete(body: &str) -> bool {
    body.contains(ENDLIST)
}

pub fn target_duration(body: &str) -> Option<u64> {
    body.lines()
        .find_map(|l| l.strip_prefix(TARGET_DURATION))
        .and_then(|v| v.trim().parse().ok())
}

pub fn media_sequence(body: &str) -> u64 {
    body.lines()
        .find_map(|l| l.strip_prefix(MEDIA_SEQUENCE))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The default subtitle track, if the master playlist marks one.
pub fn default_subtitle(master: &str) -> Option<String> {
    for line in master.lines() {
        let Some(rest) = line.trim().strip_prefix(MEDIA) else {
            continue;
        };
        if attr(rest, "TYPE") != Some("SUBTITLES".to_string()) {
            continue;
        }
        if attr(rest, "DEFAULT").as_deref() == Some("YES") {
            if let Some(uri) = attr(rest, "URI") {
                return Some(uri);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=5300000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\n\
        1080/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=854x480\n\
        480/index.m3u8\n";

    #[test]
    fn parses_and_sorts_renditions() {
        let q = parse_master(MASTER, "https://hls.test/v/abc/master.m3u8").unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].height, 1080);
        assert_eq!(q[0].label, "1080p");
        assert_eq!(q[0].url, "https://hls.test/v/abc/1080/index.m3u8");
        assert_eq!(q[1].height, 480);
        assert_eq!(q[1].bandwidth, 1_200_000);
    }

    #[test]
    fn a_single_rendition_still_parses() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1440x1080\n1080/index.m3u8\n";
        let q = parse_master(body, "https://hls.test/a/master.m3u8").unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].height, 1080, "height comes from the short side being 1080");
    }

    #[test]
    fn falls_back_to_name_then_uri() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,NAME=\"720p\"\nv720/index.m3u8\n";
        let q = parse_master(body, "https://hls.test/a/master.m3u8").unwrap();
        assert_eq!(q[0].height, 720);
        assert_eq!(q[0].label, "720p");
    }

    #[test]
    fn quoted_commas_do_not_split_attributes() {
        let q = parse_master(MASTER, "https://hls.test/a/master.m3u8").unwrap();
        assert_eq!(q[0].bandwidth, 5_300_000);
    }

    #[test]
    fn empty_master_is_an_error() {
        assert!(parse_master("#EXTM3U\n", "https://hls.test/a/master.m3u8").is_err());
    }

    #[test]
    fn absolutises_every_uri_shape() {
        assert_eq!(absolutise("https://h.test/a/b.m3u8", "c.m3u8"), "https://h.test/a/c.m3u8");
        assert_eq!(absolutise("https://h.test/a/b.m3u8", "/c.m3u8"), "https://h.test/c.m3u8");
        assert_eq!(absolutise("https://h.test/a/b.m3u8", "//x.test/c"), "https://x.test/c");
        assert_eq!(absolutise("http://h.test/a/b", "https://other/c"), "https://other/c");
    }

    #[test]
    fn rewrites_segments_to_local_urls() {
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:9.0,\nseg_00000.ts\n#EXTINF:9.0,\nseg_00001.ts\n#EXT-X-ENDLIST\n";
        let out = rewrite_media(body, "https://h.test/v/1080/index.m3u8", &|u| {
            format!("/media/seg?u={}", crate::util::url_encode(u))
        });
        assert!(out.contains("/media/seg?u=https%3A%2F%2Fh.test%2Fv%2F1080%2Fseg_00000.ts"));
        assert!(out.contains("#EXT-X-TARGETDURATION:10"));
        // The end marker stays. Dropping it turns a VOD playlist into what looks like a
        // live stream, so the player never settles and keeps refetching the manifest.
        assert!(out.contains("#EXT-X-ENDLIST"), "the VOD end marker was dropped");
    }

    #[test]
    fn rewrite_also_moves_key_uris() {
        // A media playlist carries the key URI, which has to go through the proxy too or
        // the browser cannot decrypt anything.
        let body = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://k.test/key.bin\"\n#EXTINF:4,\nseg.ts\n";
        let out = rewrite_media(body, "https://h.test/v/1080/index.m3u8", &|u| {
            format!("/p?u={}", crate::util::url_encode(u))
        });
        assert!(out.contains("#EXT-X-KEY:METHOD=AES-128,URI=\"/p?u=https%3A%2F%2Fk.test%2Fkey.bin\""));
        assert!(out.contains("/p?u=https%3A%2F%2Fh.test%2Fv%2F1080%2Fseg.ts"));
    }

    #[test]
    fn a_relative_key_uri_is_resolved_first() {
        let body = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nseg.ts\n";
        let out = rewrite_media(body, "https://h.test/v/1080/index.m3u8", &|u| {
            format!("/p?u={}", crate::util::url_encode(u))
        });
        assert!(
            out.contains("URI=\"/p?u=https%3A%2F%2Fh.test%2Fv%2F1080%2Fkey.bin\""),
            "the relative key is resolved against the playlist: {out}"
        );
    }

    #[test]
    fn complete_playlist_detection() {
        assert!(is_complete("#EXTM3U\nseg.ts\n#EXT-X-ENDLIST\n"));
        assert!(!is_complete("#EXTM3U\nseg.ts\n"));
    }

    #[test]
    fn reads_duration_and_sequence() {
        assert_eq!(target_duration("#EXT-X-TARGETDURATION:11\n"), Some(11));
        assert_eq!(media_sequence("#EXT-X-MEDIA-SEQUENCE:42\n"), 42);
        assert_eq!(media_sequence("#EXTM3U\n"), 0);
    }

    #[test]
    fn finds_the_default_subtitle() {
        let m = "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,NAME=\"en\",DEFAULT=YES,URI=\"en.m3u8\"\n#EXT-X-MEDIA:TYPE=SUBTITLES,DEFAULT=NO,URI=\"fr.m3u8\"\n";
        assert_eq!(default_subtitle(m).as_deref(), Some("en.m3u8"));
        assert_eq!(default_subtitle("#EXTM3U\n"), None);
    }

    #[test]
    fn origin_and_file_hint() {
        assert_eq!(origin("https://h.test/a/b?x=1").as_deref(), Some("https://h.test"));
        assert_eq!(file_hint("https://h.test/a/seg_1.ts?x=2"), "seg_1.ts");
        assert_eq!(file_hint("https://h.test/a/we ird.ts"), "we_ird.ts");
    }
}
