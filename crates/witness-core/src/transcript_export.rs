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
                out.push_str(&demote_headings(summary));
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
    fn headings_demote_one_level() {
        assert_eq!(
            demote_headings("# A\n## B\n####### too deep\n#hashtag\ntext"),
            "## A\n### B\n####### too deep\n#hashtag\ntext"
        );
    }
}
