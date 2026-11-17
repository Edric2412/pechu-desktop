//! The `Range:` header for audio playback, so the browser can jump around in a long recording without downloading it all.
//! (Port of the range handling in `serveAudio`, server/index.ts.)

#[derive(Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// Send bytes `start..=end` (206 Partial Content).
    Partial { start: u64, end: u64 },
    /// Nothing can be sent for this range (416).
    NotSatisfiable,
}

/// A number too big for u64 is treated as "very large": `bytes=99999999999999999999-` is past the end, not an error.
fn number(digits: &str) -> u64 {
    digits.parse().unwrap_or(u64::MAX)
}

/// Understands `bytes=START-`, `bytes=START-END` and the suffix form `bytes=-N` (the last N bytes) for a file of `size` bytes.
/// A malformed header is answered with 416 like an out-of-range one. One range only; `If-Range` is not supported.
pub fn parse_range(header: &str, size: u64) -> ByteRange {
    let Some((first, second)) = header.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
        return ByteRange::NotSatisfiable;
    };
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(first) || !digits_only(second) {
        return ByteRange::NotSatisfiable;
    }

    let last_byte = size.saturating_sub(1);
    let (start, end) = if !first.is_empty() {
        let end = if second.is_empty() { last_byte } else { number(second).min(last_byte) };
        (number(first), end)
    } else if !second.is_empty() {
        (size.saturating_sub(number(second)), last_byte) // "bytes=-N": the last N bytes
    } else {
        return ByteRange::NotSatisfiable; // just "bytes=-"
    };

    if start >= size || start > end {
        ByteRange::NotSatisfiable
    } else {
        ByteRange::Partial { start, end }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ByteRange::*;

    #[test]
    fn open_and_closed_ranges() {
        assert_eq!(parse_range("bytes=0-", 1000), Partial { start: 0, end: 999 });
        assert_eq!(parse_range("bytes=10-19", 1000), Partial { start: 10, end: 19 });
        assert_eq!(parse_range("bytes=990-5000", 1000), Partial { start: 990, end: 999 }); // end is cut to the file
        assert_eq!(parse_range("  bytes=5-5 ", 1000), Partial { start: 5, end: 5 });
    }

    #[test]
    fn suffix_range_is_the_last_bytes() {
        assert_eq!(parse_range("bytes=-100", 1000), Partial { start: 900, end: 999 });
        assert_eq!(parse_range("bytes=-5000", 1000), Partial { start: 0, end: 999 }); // more than the file: all of it
        assert_eq!(parse_range("bytes=-0", 1000), NotSatisfiable);
    }

    #[test]
    fn out_of_range_and_malformed_are_416() {
        assert_eq!(parse_range("bytes=1000-", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=2000-3000", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=20-10", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=-", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=abc-", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=1-2,5-6", 1000), NotSatisfiable); // several ranges: not supported
        assert_eq!(parse_range("items=0-1", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), NotSatisfiable); // an empty file has no bytes to give
    }

    #[test]
    fn huge_numbers_do_not_crash() {
        assert_eq!(parse_range("bytes=99999999999999999999999-", 1000), NotSatisfiable);
        assert_eq!(parse_range("bytes=0-99999999999999999999999", 1000), Partial { start: 0, end: 999 });
        assert_eq!(parse_range("bytes=-99999999999999999999999", 1000), Partial { start: 0, end: 999 });
    }
}
