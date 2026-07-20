//! Shared byte-boundary-safe string truncation helpers.
//!
//! Two modules — [`crate::overpass`] (Overpass error-body truncation) and
//! `crate::overture::cli` (Overture CLI stderr truncation) — need to clip a
//! potentially large `&str` to a fixed byte budget without splitting a UTF-8
//! code point. The two helpers below previously lived as verbatim copies in
//! each module (QA-107); they are now consolidated here as crate-private
//! utilities so the two truncation sites cannot drift apart.
//!
//! Callers typically pair [`str_prefix_at_boundary`] + [`str_suffix_at_boundary`]
//! to render a `head\n… omitted …\ntail` preview of an over-long string, using
//! [`TRUNCATE_LIMIT`] as the per-side budget (see `truncate_error_body` in
//! [`crate::overpass`] and `stderr_suffix` in `crate::overture::cli`).

/// Default byte budget for truncating a large error / stderr payload before
/// surfacing it in an `anyhow` error message. Both call sites use 4096 (8 KiB
/// total split head/tail), large enough to keep the diagnostic context OSM
/// operators need while bounding log noise from a runaway mirror response.
pub(crate) const TRUNCATE_LIMIT: usize = 4096;

/// Return the longest UTF-8-code-point-safe prefix of `s` that fits in
/// `max_bytes`. Backs up to the previous UTF-8 char boundary if `max_bytes`
/// lands mid-code-point. `max_bytes` is clamped to `s.len()`.
pub(crate) fn str_prefix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Return the longest UTF-8-code-point-safe suffix of `s` that fits in
/// `max_bytes`. Advances to the next UTF-8 char boundary if the computed
/// start lands mid-code-point. `max_bytes` is clamped to `s.len()`.
pub(crate) fn str_suffix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_returns_input_when_below_limit() {
        assert_eq!(str_prefix_at_boundary("abc", 10), "abc");
        assert_eq!(str_prefix_at_boundary("", 10), "");
    }

    #[test]
    fn prefix_clips_to_max_bytes_on_ascii() {
        assert_eq!(str_prefix_at_boundary("abcdef", 3), "abc");
    }

    #[test]
    fn prefix_backs_up_to_char_boundary_on_multibyte() {
        // 'é' is 2 bytes; asking for 1 byte must back up to 0.
        assert_eq!(str_prefix_at_boundary("éabc", 1), "");
        // Asking for 2 bytes keeps the 'é'.
        assert_eq!(str_prefix_at_boundary("éabc", 2), "é");
        // Asking for 3 bytes keeps 'é' + 'a' (4 byte boundary).
        assert_eq!(str_prefix_at_boundary("éabc", 3), "éa");
    }

    #[test]
    fn suffix_returns_input_when_below_limit() {
        assert_eq!(str_suffix_at_boundary("abc", 10), "abc");
        assert_eq!(str_suffix_at_boundary("", 10), "");
    }

    #[test]
    fn suffix_clips_to_max_bytes_on_ascii() {
        assert_eq!(str_suffix_at_boundary("abcdef", 3), "def");
    }

    #[test]
    fn suffix_advances_to_char_boundary_on_multibyte() {
        // 'é' is 2 bytes (0xC3 0xA9). `abcé` is 5 bytes: a(0) b(1) c(2) é(3-4).
        // For max_bytes=1: start = 5-1 = 4 (mid-char on 'é'), advance to 5
        // (end of string) → empty suffix.
        assert_eq!(str_suffix_at_boundary("abcé", 1), "");
        // For max_bytes=2: start = 5-2 = 3 (start of 'é') → "é".
        assert_eq!(str_suffix_at_boundary("abcé", 2), "é");
        // For max_bytes=3: start = 5-3 = 2 (start of 'c') → "cé".
        assert_eq!(str_suffix_at_boundary("abcé", 3), "cé");
    }

    #[test]
    fn head_and_tail_partition_an_overlong_string() {
        // Sanity: for an over-long ASCII string, head + tail <= limit + 1
        // (the two halves never overlap because the caller only splits when
        // the body exceeds the limit).
        let s = "x".repeat(TRUNCATE_LIMIT * 3);
        let head_len = TRUNCATE_LIMIT / 2;
        let tail_len = TRUNCATE_LIMIT - head_len;
        let head = str_prefix_at_boundary(&s, head_len);
        let tail = str_suffix_at_boundary(&s, tail_len);
        assert_eq!(head.len(), head_len);
        assert_eq!(tail.len(), tail_len);
    }
}
