//! Wall clock helpers. Everything stored is unix seconds except job timestamps,
//! which are milliseconds so progress can be ordered without ties.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Two rows written in the same millisecond would otherwise have no defined order, and
/// "most recently watched first" depends on there being one.
pub fn next_millis() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_millis();
    loop {
        let last = LAST.load(Ordering::Relaxed);
        let next = if now > last { now } else { last + 1 };
        if LAST
            .compare_exchange_weak(last, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// "3 days ago" style text. Takes a unix timestamp, not an age.
pub fn ago(ts: i64) -> String {
    let s = (now_secs() - ts).max(0);
    const UNITS: [(&str, i64); 5] = [
        ("year", 31_536_000),
        ("month", 2_592_000),
        ("day", 86_400),
        ("hour", 3_600),
        ("minute", 60),
    ];
    for (name, size) in UNITS {
        if s >= size {
            let n = s / size;
            return if n == 1 {
                format!("1 {name} ago")
            } else {
                format!("{n} {name}s ago")
            };
        }
    }
    "just now".to_string()
}

/// Seconds as `1h 04m 09s`, dropping empty leading parts.
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_picks_the_largest_fitting_unit() {
        assert_eq!(ago(now_secs()), "just now");
        assert_eq!(ago(now_secs() - 90), "1 minute ago");
        assert_eq!(ago(now_secs() - 7_200), "2 hours ago");
        assert_eq!(ago(now_secs() - 172_800), "2 days ago");
    }

    #[test]
    fn ago_never_goes_negative() {
        assert_eq!(ago(now_secs() + 500), "just now");
    }

    #[test]
    fn the_millisecond_clock_never_repeats() {
        let mut last = 0;
        for _ in 0..1000 {
            let n = next_millis();
            assert!(n > last, "{n} did not advance past {last}");
            last = n;
        }
    }

    #[test]
    fn duration_drops_empty_parts() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(75), "1m 15s");
        assert_eq!(duration(3_845), "1h 04m 05s");
        assert_eq!(duration(-5), "0s");
    }
}
