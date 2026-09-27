//! Small helpers with no better home: URL encoding, HTML entity decoding, JSON string
//! unescaping. The provider wraps HTML in JSON, so all three get used on one request.

/// Percent encode a URL for a query parameter. The unreserved set of RFC 3986 is left
/// alone, which keeps `one-piece-1` readable.
const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub fn url_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, UNRESERVED).to_string()
}

/// Percent decode, lenient about a stray `%`. A bad segment id should 404, not panic.
pub fn url_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Decode the HTML entities the provider emits inside `title` and `alt` attributes.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(end) = s[i..].find(';').filter(|e| *e <= 10) {
                let name = &s[i + 1..i + end];
                if let Some(replacement) = entity(name) {
                    out.push_str(replacement);
                    i += end + 1;
                    continue;
                }
            }
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "hellip" => "…",
        "mdash" => "\u{2014}",
        "ndash" => "\u{2013}",
        "rsquo" => "\u{2019}",
        "lsquo" => "\u{2018}",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        "middot" => "\u{b7}",
        "bull" => "\u{2022}",
        "copy" => "\u{a9}",
        "eacute" => "\u{e9}",
        "egrave" => "\u{e8}",
        _ => return None,
    })
}

/// Undo JSON string escaping for a value pulled out of a `"html":"..."` field.
/// The provider sends `\n`, `\"` and `\/` only, but handling the rest costs nothing.
pub fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push('\u{fffd}'),
                }
            }
            Some('/') => out.push('/'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// First capture group of a pattern applied to a haystack. Used for the one
/// hand written extraction the provider still needs.
pub fn capture<'a>(haystack: &'a str, start_marker: &str, end_marker: &str) -> Option<&'a str> {
    let start = haystack.find(start_marker)? + start_marker.len();
    let rest = &haystack[start..];
    let end = rest.find(end_marker)?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_round_trip() {
        let raw = "https://h.test/v/a b/seg 1.ts?x=1&y=2";
        let enc = url_encode(raw);
        assert!(!enc.contains(' '));
        assert!(!enc.contains('&'));
        assert_eq!(url_decode(&enc), raw);
    }

    #[test]
    fn decode_survives_a_broken_escape() {
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn entity_table() {
        assert_eq!(decode_entities("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("&quot;quoted&quot;"), "\"quoted\"");
        assert_eq!(decode_entities("it&#039;s"), "it&#039;s", "numeric refs pass through");
        assert_eq!(decode_entities("plain"), "plain");
        assert_eq!(decode_entities("&notarealentity;"), "&notarealentity;");
    }

    #[test]
    fn entities_keep_multibyte_text_intact() {
        assert_eq!(decode_entities("Kimetsu no Yaiba: 鬼滅の刃"), "Kimetsu no Yaiba: 鬼滅の刃");
        assert_eq!(decode_entities("鬼&滅"), "鬼&滅");
    }

    #[test]
    fn json_unescaping() {
        assert_eq!(unescape_json_string(r#"a\"b\nc\/d\\e"#), "a\"b\nc/d\\e");
        assert_eq!(unescape_json_string(r"A\u0041B"), "AAB");
        assert_eq!(unescape_json_string(r"trailing\"), "trailing");
    }

    #[test]
    fn capture_extracts_between_markers() {
        assert_eq!(capture("x<a>mid</a>", "<a>", "</a>"), Some("mid"));
        assert_eq!(capture("nothing", "<a>", "</a>"), None);
        assert_eq!(capture("unterminated <a>", "<a>", "</a>"), None);
    }
}

