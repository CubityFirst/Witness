//! Pure formatting and text helpers shared by the views (unit-tested).
//!
//! Dates use a fixed en-GB-style format instead of the WebView's locale
//! formatting (accepted in the design: `fmt_date`, Appendix C).

use chrono::{DateTime, Local, NaiveDate};
use std::ops::Range;
use witness_core::recorder::RecordingHealth;

/// `"Mon, 5 Oct 2026, 09:30"` in local time (replaces `toLocaleString`).
/// Unparseable input is returned unchanged.
pub fn fmt_date(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => at
            .with_timezone(&Local)
            .format("%a, %-d %b %Y, %H:%M")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

/// `"05/10/2026"` (replaces `toLocaleDateString`, e.g. the People trend's
/// "Week of …" tooltips).
pub fn fmt_date_short(date: NaiveDate) -> String {
    date.format("%d/%m/%Y").to_string()
}

/// Meeting durations: `"–"` when unknown, else `"1 h 5 min"`, `"12 min"`
/// or `"40 s"` (seconds rounded half up, like `Math.round`).
pub fn fmt_duration(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "–".into();
    };
    let s = (ms.max(0) + 500) / 1000;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{s} s")
    }
}

/// Clock-style offsets: `"m:ss"`, or `"h:mm:ss"` from an hour (floored;
/// negative input shows as 0).
pub fn fmt_ts(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// The recording pill's elapsed time (same shape as [`fmt_ts`]).
pub fn fmt_elapsed(ms: u64) -> String {
    fmt_ts(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// `12345` → `"12,345"` (replaces `toLocaleString()` on counts).
pub fn fmt_thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The capture-health warning shown in the top bar (the frontend's
/// `captureWarning`): the writer first, then the mic track, then loopback.
pub fn capture_warning(health: &RecordingHealth) -> Option<String> {
    if let Some(error) = &health.writer_error {
        return Some(format!("Audio writer failed: {error}"));
    }
    for (name, track) in [
        ("Microphone", &health.mic),
        ("System audio", &health.loopback),
    ] {
        if let Some(error) = &track.fatal_error {
            return Some(format!("{name} capture failed: {error}"));
        }
        if track.device_loss_count > 0 {
            return Some(format!("{name} device was lost during recording"));
        }
        if track.capture_overflow_count > 0 {
            return Some(format!(
                "{name} capture dropped {} ms of audio",
                track.capture_dropped_ms
            ));
        }
        if track.live_dropped_ms > 0 {
            return Some(format!(
                "Live captions skipped {} ms to keep recording responsive",
                track.live_dropped_ms
            ));
        }
        if track.live_disconnected {
            return Some(format!(
                "Live captions stopped receiving {}",
                name.to_lowercase()
            ));
        }
    }
    None
}

/// One run of a search snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnippetPart {
    Plain(String),
    /// A match (rendered like `<mark>`).
    Mark(String),
}

/// Split an FTS snippet into plain and marked runs. Matches are delimited
/// by `\x01` … `\x02` (never HTML, so transcript text can't inject markup).
/// This is map-ui-meetings §2.4 exactly: an unmatched `\x01` stops the
/// parse and the rest is plain text — with stray markers stripped, since
/// they would draw as tofu.
pub fn parse_snippet(text: &str) -> Vec<SnippetPart> {
    fn clean(s: &str) -> String {
        s.chars().filter(|c| *c != '\x01' && *c != '\x02').collect()
    }
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(a) = rest.find('\x01') {
        let Some(b) = rest[a + 1..].find('\x02').map(|b| a + 1 + b) else {
            break;
        };
        if a > 0 {
            parts.push(SnippetPart::Plain(clean(&rest[..a])));
        }
        parts.push(SnippetPart::Mark(clean(&rest[a + 1..b])));
        rest = &rest[b + 1..];
    }
    if !rest.is_empty() {
        parts.push(SnippetPart::Plain(clean(rest)));
    }
    parts.retain(|part| !matches!(part, SnippetPart::Plain(s) if s.is_empty()));
    parts
}

/// Search terms to highlight in a deep-linked transcript (transcript.tsx
/// `Highlighted`): split on whitespace, keep letters/digits/`'`, keep
/// tokens of ≥ 2 characters, drop the FTS operators and/or/not/near, dedupe.
pub fn highlight_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let token: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .collect();
        if token.chars().count() < 2 {
            continue;
        }
        if ["and", "or", "not", "near"].contains(&token.to_lowercase().as_str()) {
            continue;
        }
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

/// Byte ranges of `text` matching any token, case-insensitively, sorted
/// and merged (overlapping or touching ranges become one).
pub fn highlight_ranges(text: &str, tokens: &[String]) -> Vec<Range<usize>> {
    if tokens.is_empty() || text.is_empty() {
        return Vec::new();
    }
    // Lowercase the text while remembering, for every lowercase byte, the
    // original char it came from — lowercasing can change byte lengths.
    let mut lower = String::with_capacity(text.len());
    let mut origin: Vec<(usize, usize)> = Vec::with_capacity(text.len()); // (start, end) in `text`
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        for lc in c.to_lowercase() {
            let before = lower.len();
            lower.push(lc);
            origin.extend(std::iter::repeat_n((start, end), lower.len() - before));
        }
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for token in tokens {
        let needle = token.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        for (at, matched) in lower.match_indices(&needle) {
            let first = origin[at].0;
            let last = origin[at + matched.len() - 1].1;
            ranges.push(first..last);
        }
    }
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::recorder::TrackHealth;

    #[test]
    fn dates_use_the_fixed_format() {
        // Local time zone varies; check the shape via a round trip.
        let at = "2026-10-05T09:07:00+00:00";
        let local = DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(
            fmt_date(at),
            local.format("%a, %-d %b %Y, %H:%M").to_string()
        );
        assert!(fmt_date(at).contains("Oct 2026, "));
        assert_eq!(fmt_date("not a date"), "not a date");
        assert_eq!(
            fmt_date_short(NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()),
            "05/10/2026"
        );
    }

    #[test]
    fn durations_round_like_math_round() {
        assert_eq!(fmt_duration(None), "–");
        assert_eq!(fmt_duration(Some(0)), "0 s");
        assert_eq!(fmt_duration(Some(499)), "0 s");
        assert_eq!(fmt_duration(Some(500)), "1 s");
        assert_eq!(fmt_duration(Some(59_499)), "59 s");
        assert_eq!(fmt_duration(Some(59_500)), "1 min");
        assert_eq!(fmt_duration(Some(12 * 60_000 + 30_000)), "12 min");
        assert_eq!(fmt_duration(Some(3_600_000)), "1 h 0 min");
        assert_eq!(fmt_duration(Some(3_900_000)), "1 h 5 min");
    }

    #[test]
    fn timestamps_and_elapsed() {
        assert_eq!(fmt_ts(0), "0:00");
        assert_eq!(fmt_ts(9_999), "0:09");
        assert_eq!(fmt_ts(65_000), "1:05");
        assert_eq!(fmt_ts(3_599_999), "59:59");
        assert_eq!(fmt_ts(3_600_000), "1:00:00");
        assert_eq!(fmt_ts(3_725_000), "1:02:05");
        assert_eq!(fmt_ts(-5), "0:00");
        assert_eq!(fmt_elapsed(754_000), "12:34");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(1000), "1,000");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
        assert_eq!(fmt_thousands(-12_345), "-12,345");
    }

    #[test]
    fn capture_warning_priority() {
        let mut health = RecordingHealth::default();
        assert_eq!(capture_warning(&health), None);
        health.loopback.live_disconnected = true;
        assert_eq!(
            capture_warning(&health).unwrap(),
            "Live captions stopped receiving system audio"
        );
        health.mic = TrackHealth {
            capture_overflow_count: 2,
            capture_dropped_ms: 120,
            ..Default::default()
        };
        assert_eq!(
            capture_warning(&health).unwrap(),
            "Microphone capture dropped 120 ms of audio"
        );
        health.mic.device_loss_count = 1;
        assert_eq!(
            capture_warning(&health).unwrap(),
            "Microphone device was lost during recording"
        );
        health.mic.fatal_error = Some("gone".into());
        assert_eq!(
            capture_warning(&health).unwrap(),
            "Microphone capture failed: gone"
        );
        health.writer_error = Some("disk full".into());
        assert_eq!(
            capture_warning(&health).unwrap(),
            "Audio writer failed: disk full"
        );

        let live = RecordingHealth {
            mic: TrackHealth {
                live_dropped_ms: 300,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            capture_warning(&live).unwrap(),
            "Live captions skipped 300 ms to keep recording responsive"
        );
        let lost = RecordingHealth {
            mic: TrackHealth {
                live_disconnected: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            capture_warning(&lost).unwrap(),
            "Live captions stopped receiving microphone"
        );
    }

    fn plain(s: &str) -> SnippetPart {
        SnippetPart::Plain(s.into())
    }
    fn mark(s: &str) -> SnippetPart {
        SnippetPart::Mark(s.into())
    }

    #[test]
    fn snippets_split_into_marks() {
        assert_eq!(parse_snippet(""), vec![]);
        assert_eq!(parse_snippet("no marks"), vec![plain("no marks")]);
        assert_eq!(
            parse_snippet("… the \x01budget\x02 for \x01Q3\x02"),
            vec![plain("… the "), mark("budget"), plain(" for "), mark("Q3")]
        );
        assert_eq!(parse_snippet("\x01a\x02"), vec![mark("a")]);
    }

    #[test]
    fn malformed_snippets_stop_the_parse_and_strip_markers() {
        // Unmatched opener: the rest is plain, minus the control character.
        assert_eq!(
            parse_snippet("x \x01y\x02 z \x01open"),
            vec![plain("x "), mark("y"), plain(" z open")]
        );
        // A stray closer before any opener stays in the plain run, stripped.
        assert_eq!(parse_snippet("a\x02b"), vec![plain("ab")]);
        // A nested opener inside a mark is stripped from the mark.
        assert_eq!(
            parse_snippet("\x01a\x01b\x02c"),
            vec![mark("ab"), plain("c")]
        );
    }

    #[test]
    fn highlight_tokens_follow_the_frontend_rules() {
        assert_eq!(
            highlight_tokens("  budget AND \"q3\" or near x it's NOT budget "),
            vec!["budget".to_string(), "q3".into(), "it's".into()]
        );
        assert!(highlight_tokens("a + -").is_empty());
        assert_eq!(
            highlight_tokens("Ünïcode 会议"),
            vec!["Ünïcode".to_string(), "会议".into()]
        );
    }

    #[test]
    fn highlight_ranges_are_case_insensitive_and_merged() {
        let tokens = highlight_tokens("budget get");
        let text = "Budget: the BUDGET got budgeted";
        let ranges = highlight_ranges(text, &tokens);
        let found: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, ["Budget", "BUDGET", "budget"]);
        assert!(highlight_ranges(text, &[]).is_empty());

        // Touching / overlapping matches merge.
        let ranges = highlight_ranges("abcd", &["ab".to_string(), "bc".into(), "cd".into()]);
        assert_eq!(ranges, vec![0..4]);
    }

    #[test]
    fn highlight_ranges_map_back_through_length_changing_lowercase() {
        // 'İ' (2 bytes) lowercases to "i̇" (3 bytes): offsets must map back.
        let text = "İstanbul and ISTANBUL";
        let ranges = highlight_ranges(text, &["stanbul".to_string()]);
        let found: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, ["stanbul", "STANBUL"]);
        let ranges = highlight_ranges("ÀB àb", &["àb".to_string()]);
        assert_eq!(ranges.len(), 2);
    }
}
