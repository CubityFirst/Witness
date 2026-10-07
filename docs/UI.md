# Witness UI design language

The native UI (egui, `crates/witness/src/ui/`) follows one calm, dark,
modern look. These rules keep it consistent as views change; the tokens
live in `ui/theme.rs` and the building blocks in the "Rework kit" section
of `ui/widgets.rs`.

## Principles

1. **Fewer boxes.** Lists are flat rows on the background with a hover
   fill (`widgets::list_row`); grouping comes from whitespace and small
   headers (`section_header`, `day_header`), not borders. Raised surfaces
   (`BG_RAISED`, hairline `BORDER`, radius 12) are for genuinely grouped
   controls: Settings groups (`widgets::group` + `group_divider`), the
   player bar, popups, the confirm modal and toasts.
2. **Typography carries hierarchy.** Page title 20 semibold · section
   header 12.5 semibold muted (sentence case) · body 14 · meta 12 muted ·
   tertiary 12 `TEXT_FAINT`.
3. **Progressive disclosure.** Row actions appear on hover / keyboard
   focus; status icons only for non-default states (a transcribed meeting
   shows nothing).
4. **Responsive by construction — no overflow at any width ≥ 380 pt**
   (the minimum window width). Single-line texts elide with `…` and a
   tooltip (`widgets::elide`); control groups wrap or stack below
   `theme::COMPACT_BELOW` (460) via `widgets::is_compact`; nothing gets a
   fixed width wider than its space. The top bar never wraps: it degrades
   (nav icon-only → compact pill → dot) instead.
5. **Rhythm.** Spacing 4/8/12/16/24 (`theme::S1..S5`); content column max
   760 pt (`CONTENT_MAX`), side padding 16 up to 640 pt wide, 24 beyond
   (`widgets::pad_x`); radius 6/8/12 (`R_SM/R_MD/R_LG`), pills fully round.
6. **One accent** (`ACCENT`) for primary actions, selection, focus,
   progress and search hits (`MARK_BG`). Red only for record/destructive,
   green/amber only for status. Speaker colours (`chip_color`) appear as
   text, legend dots and avatar tints (`speaker_text`, `lighten`), never as
   a pill behind every line.

## Layout plumbing

- Every view draws its own full-width scroller with
  `widgets::scroll_column` (or `padded_column` inside a custom scroller,
  as the virtualised transcript does). Scroll bars are **floating**: they
  overlay the page padding and take no width, so a view's column lines up
  exactly with the shell's search box and banners.
- `centered_column` / `padded_column` restore the normal horizontal item
  spacing for their content.
- `list_row` senses through its scope (registered before the content), so
  buttons inside a row keep their clicks.

## Checking a change visually

Debug builds can render any view off screen at any size:

```powershell
$env:WITNESS_SETTINGS = '<scratch data>\witness-settings.toml'  # auto_record = false
$env:WITNESS_DEBUG_START_VIEW = 'transcript:10'
$env:WITNESS_DEBUG_SIZE = '380x760'
$env:WITNESS_DEBUG_SNAPSHOT = 'C:\shots\transcript-380.bmp'
.\target\debug\witness.exe   # cloaked, never activated; writes the BMP and exits
```

The run is DWM-cloaked and never activated, takes no tray icon or hotkeys,
and uses per-process single-instance names, so it runs beside a real
instance. Check every change at **380, 460, 640 and 1000** wide. More
debug-only hooks reach states without input: `WITNESS_DEBUG_FAKE_RECORDING`
(`1|warn|busy`), `WITNESS_DEBUG_MODEL_BANNER`, `WITNESS_DEBUG_CONFIRM`,
`WITNESS_DEBUG_TOASTS`, `WITNESS_DEBUG_BIN`, `WITNESS_DEBUG_FILTERS`,
`WITNESS_DEBUG_EXPAND_PERSON=<i>`, `WITNESS_DEBUG_SCROLL=<pt>` (Settings),
`WITNESS_DEBUG_TRANSCRIPT_{NOTES,SEGMENT,HIGHLIGHT,MENU,VOLUME,RENAME}`,
`WITNESS_DEBUG_SYNTHETIC`.
