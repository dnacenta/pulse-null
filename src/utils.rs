/// Maximum bytes for summary truncation (e.g., event summaries, response previews).
pub const SUMMARY_TRUNCATE_LEN: usize = 300;

/// Maximum bytes for logbook entry summaries.
pub const LOGBOOK_TRUNCATE_LEN: usize = 500;

/// Maximum bytes for full content truncation (e.g., tool results, page content).
pub const CONTENT_TRUNCATE_LEN: usize = 2000;

/// Maximum bytes for topic display strings.
pub const TOPIC_TRUNCATE_LEN: usize = 80;

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 char boundaries.
///
/// Returns the original string if it fits within `max_bytes`.
/// Otherwise, finds the largest valid char boundary at or before `max_bytes`
/// and returns the slice up to that point.
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_no_truncation() {
        let s = "hello";
        assert_eq!(safe_truncate(s, 10), "hello");
    }

    #[test]
    fn ascii_truncation() {
        let s = "hello world";
        assert_eq!(safe_truncate(s, 5), "hello");
    }

    #[test]
    fn multibyte_boundary() {
        // Each emoji is 4 bytes
        let s = "\u{1F600}\u{1F601}\u{1F602}"; // 12 bytes total
                                               // Requesting 6 bytes should give us the first emoji (4 bytes) + not split the second
        assert_eq!(safe_truncate(s, 6), "\u{1F600}");
        // Requesting 8 bytes should give us two emojis (8 bytes)
        assert_eq!(safe_truncate(s, 8), "\u{1F600}\u{1F601}");
    }

    #[test]
    fn two_byte_chars() {
        let s = "\u{00E9}\u{00E9}\u{00E9}"; // 6 bytes (each e-acute is 2 bytes)
        assert_eq!(safe_truncate(s, 3), "\u{00E9}"); // can't split second char
        assert_eq!(safe_truncate(s, 4), "\u{00E9}\u{00E9}");
    }

    #[test]
    fn zero_max() {
        assert_eq!(safe_truncate("hello", 0), "");
    }

    #[test]
    fn empty_string() {
        assert_eq!(safe_truncate("", 10), "");
    }
}
