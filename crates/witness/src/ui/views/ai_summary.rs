//! The transcript's "Summary" disclosure: the AI assistant's notes for the
//! meeting (`meetings.ai_summary`), rendered from their Markdown subset
//! (headings, bullets, numbered items, **bold**), with Generate /
//! Regenerate, "Copy to notes", the running stage and the last error.
//!
//! The panel is a pure renderer: the transcript view owns the open state,
//! passes the meeting's data in and acts on the returned [`SummaryAction`]
//! (queue a job through the core, append to the notes draft, open
//! Settings). Without a configured AI service it shows the stored summary
//! (if any) and a quiet pointer to Settings instead of the buttons.

use crate::model::AiTaskState;
use crate::ui::format::fmt_date;
use crate::ui::icons::{self, ph};
use crate::ui::theme::{self, pal};
use crate::ui::widgets;
use eframe::egui::{self, CornerRadius, Frame, Id, Margin, Sense, Stroke};

/// The disclosure row's height (the notes row's).
const ROW_H: f32 = 32.0;
const ROW_PAD_X: f32 = 10.0;
const BODY_SIZE: f32 = 13.5;
const BULLET_INDENT: f32 = 14.0;

/// What the panel shows this pass.
pub(crate) struct SummaryInput<'a> {
    pub meeting_id: i64,
    pub summary: &'a str,
    /// Written for a previous transcript of this meeting.
    pub stale: bool,
    pub generated_at: Option<&'a str>,
    pub has_transcript: bool,
    /// The AI service is on and set up (the buttons show).
    pub configured: bool,
    pub task: Option<&'a AiTaskState>,
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryAction {
    None,
    Generate,
    CopyToNotes,
    OpenSettings,
}

/// Whether the panel has anything to say for this meeting at all.
pub(crate) fn shows(input: &SummaryInput) -> bool {
    !input.summary.trim().is_empty() || input.has_transcript || input.task.is_some()
}

/// The disclosure row and (when `open`) its body.
pub(crate) fn summary_panel(
    ui: &mut egui::Ui,
    open: &mut bool,
    input: &SummaryInput,
    md: &mut MdCache,
) -> SummaryAction {
    let busy = match input.task {
        Some(AiTaskState::Busy(stage)) => Some(stage.as_str()),
        _ => None,
    };
    let failed = match input.task {
        Some(AiTaskState::Failed(error)) => Some(error.as_str()),
        _ => None,
    };
    let summary = input.summary.trim();
    let indicator = if busy.is_some() {
        Some("Generating…")
    } else if failed.is_some() {
        Some("Failed")
    } else if input.stale && !summary.is_empty() {
        Some("From an earlier transcript")
    } else {
        None
    };
    let preview = if *open {
        None
    } else if !summary.is_empty() {
        md.parsed(summary).1.map(str::to_owned)
    } else if input.configured {
        Some("Not written yet".to_string())
    } else {
        Some("AI assistant is off".to_string())
    };

    let (rect, header) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    header.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, *open, "Summary")
    });
    if ui.is_rect_visible(rect) {
        let ring = header.has_focus() && widgets::focus_visible(ui.ctx());
        let hot = header.hovered() || ring;
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_hover);
        }
        let cy = rect.center().y;
        let mut x = rect.left() + ROW_PAD_X;
        let caret = if *open {
            ph::CARET_DOWN
        } else {
            ph::CARET_RIGHT
        };
        let put = |x: &mut f32, galley: std::sync::Arc<egui::Galley>, gap: f32| {
            let width = galley.size().x;
            painter.galley(
                egui::pos2(*x, cy - galley.size().y / 2.0),
                galley,
                pal().text,
            );
            *x += width + gap;
        };
        put(
            &mut x,
            painter.layout_no_wrap(caret.into(), theme::regular(12.0), pal().text_muted),
            6.0,
        );
        let label_color = if hot { pal().text } else { pal().text_muted };
        put(
            &mut x,
            painter.layout_no_wrap("Summary".into(), theme::semibold(13.0), label_color),
            8.0,
        );
        if busy.is_some() {
            let spin =
                egui::Rect::from_center_size(egui::pos2(x + 6.0, cy), egui::vec2(12.0, 12.0));
            icons::paint_spinner(ui, spin, 12.0, pal().accent);
            x += 18.0;
        }
        if let Some(text) = indicator {
            let color = if failed.is_some() {
                pal().rec_light
            } else {
                pal().text_faint
            };
            put(
                &mut x,
                painter.layout_no_wrap(text.into(), theme::regular(12.0), color),
                12.0,
            );
        }
        let room = rect.right() - ROW_PAD_X - x;
        if let Some(line) = preview.filter(|_| room > 40.0) {
            let galley = elided(ui, &line, room);
            painter.galley(
                egui::pos2(x, cy - galley.size().y / 2.0),
                galley,
                pal().text_faint,
            );
        }
        if ring {
            ui.painter().rect_stroke(
                rect.shrink(1.0),
                CornerRadius::same(theme::R_MD),
                Stroke::new(2.0, pal().focus),
                egui::StrokeKind::Inside,
            );
        }
    }
    if header.clicked() {
        *open = !*open;
    }
    widgets::announce(
        ui.ctx(),
        Id::new(("ai-summary-status", input.meeting_id)),
        indicator.unwrap_or(""),
        widgets::Politeness::Polite,
    );
    if !*open {
        widgets::gap(ui, theme::S1);
        return SummaryAction::None;
    }

    widgets::gap(ui, theme::S1);
    let mut action = SummaryAction::None;
    if let Some(error) = failed {
        widgets::inline_warning(ui, &format!("The summary could not be written: {error}"));
        widgets::gap(ui, theme::S2);
    }
    if !summary.is_empty() {
        Frame::new()
            .fill(pal().bg_raised)
            .stroke(Stroke::new(1.0, pal().border))
            .corner_radius(CornerRadius::same(theme::R_MD))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                render_blocks(ui, md.parsed(summary).0);
            });
        widgets::gap(ui, theme::S2);
    } else if let Some(stage) = busy {
        Frame::new()
            .fill(pal().bg_raised)
            .stroke(Stroke::new(1.0, pal().border))
            .corner_radius(CornerRadius::same(theme::R_MD))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 8.0;
                muted(ui, &format!("{stage}…").replace("……", "…"));
                let width = ui.available_width();
                for share in [0.42, 0.9, 0.75, 0.82] {
                    widgets::skeleton(ui, egui::vec2((width * share).max(40.0), 10.0));
                }
            });
        widgets::gap(ui, theme::S2);
    } else if input.configured {
        muted(
            ui,
            "Let the AI assistant write a short summary, key points, decisions and action items from the transcript.",
        );
        widgets::gap(ui, theme::S2);
    } else {
        muted(ui, &provenance(input));
        widgets::gap(ui, theme::S2);
    }

    // Actions (nothing to start while a job runs), then the provenance.
    let generate = input.configured && busy.is_none();
    if generate || !summary.is_empty() || !input.configured {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(theme::S2, theme::S2);
            if generate {
                let clicked = ui
                    .add_enabled_ui(input.has_transcript, |ui| {
                        if summary.is_empty() {
                            widgets::btn_primary(
                                ui,
                                if failed.is_some() {
                                    "Try again"
                                } else {
                                    "Generate summary"
                                },
                            )
                        } else {
                            widgets::btn(ui, "Regenerate")
                        }
                    })
                    .inner
                    .clicked();
                if clicked {
                    action = SummaryAction::Generate;
                }
            }
            if !summary.is_empty() && widgets::btn(ui, "Copy to notes").clicked() {
                action = SummaryAction::CopyToNotes;
            }
            if !input.configured && widgets::btn(ui, "Set up the AI assistant…").clicked() {
                action = SummaryAction::OpenSettings;
            }
        });
    }
    if !summary.is_empty() {
        widgets::gap(ui, theme::S1);
        ui.add(
            egui::Label::new(
                egui::RichText::new(provenance(input))
                    .font(theme::regular(12.0))
                    .color(pal().text_faint),
            )
            .wrap(),
        );
    }
    widgets::gap(ui, theme::S3);
    action
}

/// "AI-generated · 1 Aug 2026. Check it before relying on it." or the
/// off-state pointer.
fn provenance(input: &SummaryInput) -> String {
    if input.summary.trim().is_empty() {
        return "Off by default. Once you set up a service in Settings → AI assistant, Witness can summarize meetings; nothing is sent before that.".into();
    }
    let mut line = String::from("AI-generated");
    if let Some(at) = input.generated_at {
        line.push_str(&format!(" · {}", fmt_date(at)));
    }
    if input.stale {
        line.push_str(" · written for an earlier transcript");
    }
    line.push_str(". Check it before relying on it.");
    line
}

fn muted(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::regular(12.5))
                .color(pal().text_muted),
        )
        .wrap(),
    );
}

fn elided(ui: &egui::Ui, text: &str, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_string(),
        theme::regular(12.5),
        pal().text_faint,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    ui.painter().layout_job(job)
}

/// The parsed summary, re-parsed only when its text changes (not every
/// frame): the blocks and the closed-row preview line.
#[derive(Debug, Default)]
pub(crate) struct MdCache {
    text: String,
    blocks: Vec<Block>,
    preview: Option<String>,
}

impl MdCache {
    pub(crate) fn parsed(&mut self, markdown: &str) -> (&[Block], Option<&str>) {
        if self.text != markdown || (self.blocks.is_empty() && !markdown.is_empty()) {
            self.text = markdown.to_string();
            self.blocks = md_blocks(markdown);
            self.preview = first_text_line(&self.blocks);
        }
        (&self.blocks, self.preview.as_deref())
    }
}

/// The first line with text, without Markdown markers (closed preview).
fn first_text_line(blocks: &[Block]) -> Option<String> {
    blocks.iter().find_map(|block| match block {
        Block::Paragraph(text) | Block::Bullet { text, .. } => {
            Some(strip_inline(text)).filter(|t| !t.is_empty())
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// The Markdown subset (pure)

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// `#`…`######` (level 1–6).
    Heading(usize, String),
    /// `-`, `*`, `+` or `1.` items; `marker` is "•" or "1." etc.
    Bullet {
        depth: usize,
        marker: String,
        text: String,
    },
    Paragraph(String),
    /// A blank line between blocks.
    Gap,
}

/// Line-based parse of the subset the notes prompt asks for.
pub(crate) fn md_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    for raw in markdown.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.is_empty() {
            if !matches!(blocks.last(), Some(Block::Gap) | None) {
                blocks.push(Block::Gap);
            }
            continue;
        }
        if line.starts_with("```") {
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            blocks.push(Block::Heading(hashes, line[hashes..].trim().to_string()));
            continue;
        }
        let depth = indent / 2;
        if let Some(rest) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| line.strip_prefix(marker))
        {
            let rest = rest
                .strip_prefix("[ ] ")
                .or_else(|| rest.strip_prefix("[x] "))
                .unwrap_or(rest);
            blocks.push(Block::Bullet {
                depth,
                marker: "•".into(),
                text: rest.trim().to_string(),
            });
            continue;
        }
        let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits <= 3 {
            let after = &line[digits..];
            if let Some(rest) = after
                .strip_prefix(". ")
                .or_else(|| after.strip_prefix(") "))
            {
                blocks.push(Block::Bullet {
                    depth,
                    marker: format!("{}.", &line[..digits]),
                    text: rest.trim().to_string(),
                });
                continue;
            }
        }
        blocks.push(Block::Paragraph(line.to_string()));
    }
    while matches!(blocks.last(), Some(Block::Gap)) {
        blocks.pop();
    }
    blocks
}

/// `**bold**` runs as (text, bold); backticks and `__` are dropped.
pub(crate) fn inline_runs(text: &str) -> Vec<(String, bool)> {
    let text = text.replace('`', "").replace("__", "");
    let mut runs = Vec::new();
    for (i, part) in text.split("**").enumerate() {
        if !part.is_empty() {
            runs.push((part.to_string(), i % 2 == 1));
        }
    }
    runs
}

fn strip_inline(text: &str) -> String {
    inline_runs(text)
        .into_iter()
        .map(|(t, _)| t)
        .collect::<String>()
        .trim()
        .to_string()
}

fn inline_job(text: &str, size: f32, color: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for (run, bold) in inline_runs(text) {
        let font = if bold {
            theme::semibold(size)
        } else {
            theme::regular(size)
        };
        job.append(
            &run,
            0.0,
            egui::TextFormat {
                font_id: font,
                color,
                line_height: Some(size * 1.45),
                ..Default::default()
            },
        );
    }
    job
}

fn render_blocks(ui: &mut egui::Ui, blocks: &[Block]) {
    for (index, block) in blocks.iter().enumerate() {
        match block {
            Block::Heading(level, text) => {
                if index > 0 {
                    ui.add_space(theme::S1);
                }
                let size = if *level <= 2 { 14.5 } else { 13.5 };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(strip_inline(text))
                            .font(theme::semibold(size))
                            .color(pal().text),
                    )
                    .wrap(),
                );
            }
            Block::Bullet {
                depth,
                marker,
                text,
            } => {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_space(*depth as f32 * BULLET_INDENT);
                    ui.label(
                        egui::RichText::new(marker.as_str())
                            .font(theme::regular(BODY_SIZE))
                            .color(pal().text_muted),
                    );
                    ui.add(egui::Label::new(inline_job(text, BODY_SIZE, pal().text)).wrap());
                });
            }
            Block::Paragraph(text) => {
                ui.add(egui::Label::new(inline_job(text, BODY_SIZE, pal().text)).wrap());
            }
            Block::Gap => ui.add_space(2.0),
        }
    }
}

/// A sample summary for debug snapshots (`WITNESS_DEBUG_AI_SUMMARY=sample`).
#[cfg_attr(not(any(debug_assertions, test)), allow(dead_code))]
pub(crate) const SAMPLE_SUMMARY: &str = "## Summary\nThe team reviewed the Q3 roadmap and agreed to move the billing migration to October so the mobile release can ship first.\n\n## Key points\n- Mobile release is feature-complete; **QA** needs one more week.\n- Billing migration depends on the new invoicing API, which is late.\n- Support volume rose 12% after the pricing change.\n\n## Decisions\n- Ship the mobile release on **15 September**.\n- Postpone the billing migration to October.\n\n## Action items\n- **Priya**: publish the updated roadmap by Friday.\n- **Marco**: confirm the invoicing API date with the vendor.\n- **Unassigned**: draft the customer email about the pricing FAQ.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_subset_parses() {
        let blocks = md_blocks(
            "## Summary\nA short one.\n\n\n## Action items\n- **Ann** — ship\n  - sub point\n1. first\n2) second\n- [ ] todo\n```\n#hashtag\n\n",
        );
        assert_eq!(
            blocks,
            vec![
                Block::Heading(2, "Summary".into()),
                Block::Paragraph("A short one.".into()),
                Block::Gap,
                Block::Heading(2, "Action items".into()),
                Block::Bullet {
                    depth: 0,
                    marker: "•".into(),
                    text: "**Ann** — ship".into()
                },
                Block::Bullet {
                    depth: 1,
                    marker: "•".into(),
                    text: "sub point".into()
                },
                Block::Bullet {
                    depth: 0,
                    marker: "1.".into(),
                    text: "first".into()
                },
                Block::Bullet {
                    depth: 0,
                    marker: "2.".into(),
                    text: "second".into()
                },
                Block::Bullet {
                    depth: 0,
                    marker: "•".into(),
                    text: "todo".into()
                },
                Block::Paragraph("#hashtag".into()),
            ]
        );
        assert!(md_blocks("").is_empty());
    }

    #[test]
    fn bold_runs_alternate() {
        assert_eq!(
            inline_runs("**Ann** — ship `v2`"),
            vec![("Ann".into(), true), (" — ship v2".into(), false)]
        );
        assert_eq!(inline_runs("plain"), vec![("plain".into(), false)]);
        assert_eq!(first_text_line(&md_blocks(SAMPLE_SUMMARY)).unwrap(), "The team reviewed the Q3 roadmap and agreed to move the billing migration to October so the mobile release can ship first.");
        assert_eq!(first_text_line(&md_blocks("## Only a heading")), None);
    }

    #[test]
    fn the_summary_is_parsed_once_per_text() {
        let mut cache = MdCache::default();
        let first = cache.parsed(SAMPLE_SUMMARY).0.as_ptr();
        assert_eq!(cache.parsed(SAMPLE_SUMMARY).0.as_ptr(), first, "reused");
        assert!(cache
            .parsed(SAMPLE_SUMMARY)
            .1
            .unwrap()
            .starts_with("The team"));
        let (blocks, preview) = cache.parsed("- other");
        assert_eq!(blocks.len(), 1);
        assert_eq!(preview, Some("other"));
    }

    #[test]
    fn provenance_says_where_it_came_from() {
        let input = |summary, stale, configured| SummaryInput {
            meeting_id: 1,
            summary,
            stale,
            generated_at: None,
            has_transcript: true,
            configured,
            task: None,
        };
        assert!(provenance(&input("x", false, true)).starts_with("AI-generated. Check"));
        assert!(provenance(&input("x", true, true)).contains("earlier transcript"));
        assert!(provenance(&input("", false, false)).contains("nothing is sent"));
        assert!(shows(&input("", false, false)));
        let none = SummaryInput {
            has_transcript: false,
            ..input("", false, true)
        };
        assert!(!shows(&none));
    }
}
