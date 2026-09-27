//! HTTP byte ranges. The player seeks with them, so a wrong answer means a broken seek bar.

/// A parsed `Range: bytes=...` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// First `end` bytes, counted from the start.
    FromStart { end: u64 },
    /// Everything from `start` on.
    From { start: u64 },
    /// A closed interval, both ends inclusive.
    Between { start: u64, end: u64 },
}

/// Returns `None` for a header we should ignore, which makes the server send the whole body,
/// and `Unsatisfiable` for a range past the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    None,
    Satisfiable(Range),
    /// Valid syntax, but nothing in the file matches it.
    Unsatisfiable { requested_start: u64 },
}

/// Only the first range of a possibly multi range header is honoured. That is allowed
/// by the spec and is what every browser in practice sends for media.
pub fn parse(header: &str) -> Parsed {
    // Whitespace around the `=` is tolerated: it costs nothing and proxies add it.
    let spec = header.trim();
    let Some((unit, spec)) = spec.split_once('=') else {
        return Parsed::None;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Parsed::None;
    }
    let first = spec.split(',').next().unwrap_or("").trim();
    let Some((lhs, rhs)) = first.split_once('-') else {
        return Parsed::None;
    };
    let (lhs, rhs) = (lhs.trim(), rhs.trim());

    match (lhs.is_empty(), rhs.is_empty()) {
        (true, true) => Parsed::None,
        (true, false) => match rhs.parse::<u64>() {
            // Suffix range: the last N bytes.
            Ok(0) => Parsed::Unsatisfiable { requested_start: 0 },
            Ok(n) => Parsed::Satisfiable(Range::FromStart { end: n }),
            Err(_) => Parsed::None,
        },
        (false, true) => match lhs.parse::<u64>() {
            Ok(start) => Parsed::Satisfiable(Range::From { start }),
            Err(_) => Parsed::None,
        },
        (false, false) => {
            let (Ok(start), Ok(end)) = (lhs.parse::<u64>(), rhs.parse::<u64>()) else {
                return Parsed::None;
            };
            if end < start {
                return Parsed::None;
            }
            Parsed::Satisfiable(Range::Between { start, end })
        }
    }
}

/// Turn a range plus a known file size into the slice to send, and the status to send with it.
pub fn resolve(parsed: Parsed, size: u64) -> Option<(u64, u64, u16)> {
    let last = size.checked_sub(1)?;
    let (start, end) = match parsed {
        Parsed::None => return Some((0, last, 200)),
        Parsed::Unsatisfiable { .. } => return None,
        Parsed::Satisfiable(r) => match r {
            Range::FromStart { end: n } => (size.saturating_sub(n), last),
            Range::From { start } => (start, last),
            Range::Between { start, end } => (start, end.min(last)),
        },
    };
    if size == 0 || start >= size || start > end {
        return None;
    }
    let status = if parsed == Parsed::None { 200 } else { 206 };
    Some((start, end.min(last), status))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 1000;

    #[test]
    fn open_ended_range() {
        assert_eq!(parse("bytes=100-"), Parsed::Satisfiable(Range::From { start: 100 }));
        assert_eq!(resolve(parse("bytes=100-"), SIZE), Some((100, 999, 206)));
    }

    #[test]
    fn closed_range() {
        assert_eq!(resolve(parse("bytes=0-499"), SIZE), Some((0, 499, 206)));
    }

    #[test]
    fn suffix_range_takes_the_tail() {
        assert_eq!(resolve(parse("bytes=-500"), SIZE), Some((500, 999, 206)));
    }

    #[test]
    fn end_beyond_the_file_is_clamped() {
        assert_eq!(resolve(parse("bytes=900-5000"), SIZE), Some((900, 999, 206)));
    }

    #[test]
    fn no_header_sends_everything() {
        assert_eq!(parse("items=0-10"), Parsed::None);
        assert_eq!(resolve(Parsed::None, SIZE), Some((0, 999, 200)));
    }

    #[test]
    fn unsatisfiable_ranges_have_no_answer() {
        assert_eq!(resolve(parse("bytes=5000-6000"), SIZE), None);
        assert_eq!(resolve(parse("bytes=2000-"), SIZE), None);
        assert_eq!(resolve(Parsed::Unsatisfiable { requested_start: 0 }, SIZE), None);
    }

    #[test]
    fn reversed_and_junk_ranges_are_ignored() {
        assert_eq!(parse("bytes=500-100"), Parsed::None);
        assert_eq!(parse("bytes=abc-def"), Parsed::None);
        assert_eq!(parse("bytes=-"), Parsed::None);
        assert_eq!(parse(""), Parsed::None);
        assert_eq!(parse("bytes=-0"), Parsed::Unsatisfiable { requested_start: 0 });
    }

    #[test]
    fn only_the_first_of_a_multi_range_is_used() {
        assert_eq!(resolve(parse("bytes=0-99,200-299"), SIZE), Some((0, 99, 206)));
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(resolve(parse("  bytes = 0 - 9 "), SIZE), Some((0, 9, 206)));
    }

    #[test]
    fn an_empty_file_cannot_be_ranged() {
        assert_eq!(resolve(parse("bytes=0-"), 0), None);
    }
}
