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

    let mut out = String::new();
    match format {
        ExportFormat::Markdown => {
            out.push_str(&format!("# {}\n\n", meeting.title));
            out.push_str(&format!("*{}*", meeting.started_at));
            if let Some(d) = meeting.duration_ms {
                out.push_str(&format!(" · {}", clock(d)));
            }
            out.push_str("\n\n");
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
