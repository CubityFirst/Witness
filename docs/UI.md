# Witness UI design language

The native UI (egui, `crates/witness/src/ui/`) follows one calm, modern,
Windows 11–native look in a light and a dark theme. These rules keep it
consistent as views change; the tokens live in `ui/theme.rs` and the
building blocks in the "Rework kit" section of `ui/widgets.rs`.

## Principles

1. **Fewer boxes.** Lists are flat rows on the background with a hover
   fill (`widgets::list_row`); grouping comes from whitespace and small
   headers (`section_header`, `day_header`), not borders. Raised surfaces
   (`bg_raised`, hairline `border`, radius 12) are for genuinely grouped
   controls: Settings groups (`widgets::group` + `group_divider`), the
   player bar, popups, the confirm modal, the command palette and toasts.
2. **Typography carries hierarchy.** Segoe UI Variable (Windows 11; classic
   Segoe UI elsewhere and as the non-Latin fallback). Page title 20
   semibold · section header 12.5 semibold muted (sentence case) · body 14
   · meta 12 muted · tertiary 12 `text_faint`. Time labels reserve their
   width with "0" digits so they never jitter.
3. **Progressive disclosure.** Row actions appear on hover / keyboard
   focus; status icons only for non-default states (a transcribed meeting
   shows nothing).
4. **Responsive by construction — no overflow at any width ≥ 380 pt**
   (the minimum window width). Single-line texts elide with `…` and a
   tooltip (`widgets::elide`); control groups wrap or stack below
   `theme::COMPACT_BELOW` (460) via `widgets::is_compact`; nothing gets a
   fixed width wider than its space. The top bar never wraps: it degrades
   (nav icon-only → compact pill → dot) instead. `theme::narrow` measures
   the split column being drawn, not the window.
5. **Rhythm.** Spacing 4/8/12/16/24 (`theme::S1..S5`); content column max
   760 pt (`CONTENT_MAX`), side padding 16 up to 640 pt wide, 24 beyond
   (`widgets::pad_x`); radius 6/8/12 (`R_SM/R_MD/R_LG`), pills fully round.
6. **One accent** — Windows' accent colour by default (Appearance →
   Accent can pick Witness blue): a light shade in dark mode, a dark one in
   light mode, text on it in `on_accent` (contrast-picked). It marks
   primary actions, selection, focus, progress and search hits
   (`mark_bg`). Red only for record/destructive, green/amber only for
   status. Speaker colours (`chip_color`) appear as text, legend dots,
   timeline spans and avatar tints (`speaker_text` / `ink`), never as a
   pill behind every line.
7. **Motion is quiet.** Page changes fade in over 150 ms with a 6 pt rise;
   a new transcript in the split cross-fades (120 ms); skeletons shimmer
   while a view first loads. Nothing animates with Windows animations off
   or while the window is hidden or minimized.

## Themes

`theme::Palette` holds every colour; read it with `pal()` at paint time,
never cache colours in consts or statics. The shell resolves it each frame
from the Appearance prefs (`ui_state.rs`: System / Light / Dark, accent
source, Mica) and Windows' app mode and accent ramp (polled every 2 s), and
`theme::apply` restyles on a change. Windows 11 22H2+ can show the **Mica**
backdrop: the window is created transparent and `window_bg` (the shell's
panels, `clear_color`) turns transparent; opaque masks keep using `bg`.
The live-caption overlay stays dark in both themes. Check every change in
**both** themes (`WITNESS_DEBUG_THEME=light|dark`).

## Layout

- Every view draws its own full-width scroller with
  `widgets::scroll_column` (or `padded_column` inside a custom scroller,
  as the virtualised transcript does). Scroll bars are **floating**: they
  overlay the page padding and take no width, so a view's column lines up
  exactly with the shell's search box and banners.
- **Split layout** (`ui::PaneLayout`): from 900 pt of content width,
  Meetings / Search and a transcript opened from them show side by side —
  the list column 360 pt (drag the hairline divider between 300 and 520,
  double-click to reset; remembered in `ui-state.toml`), the transcript
  beside it or a "Select a meeting" placeholder. Views read `cx.pane`
  (`PaneRole::List { selected }` / `Detail` / `Full`): the list highlights
  the open meeting, the transcript drops its Back button. The shell keeps
  the list instance alive behind a transcript, so list `on_leave` must be
  non-destructive.
- `centered_column` / `padded_column` restore the normal horizontal item
  spacing for their content.
- `list_row` senses through its scope (registered before the content), so
  buttons inside a row keep their clicks.
- Loading states use `widgets::skeleton` shaped like the real layout;
  empty states have an icon, a title, one sentence and an action.

## Keyboard

- **Ctrl+K** — command palette (actions, meetings, people; fuzzy-ranked).
- **Ctrl+F** — find in the open transcript (Enter / Shift+Enter, F3); in
  lists it focuses the search box, as does **/**.
- Lists (Meetings, the bin, Search hits, People): **↑/↓** or **j/k**,
  **Home/End**, **Enter** opens / expands; Meetings also **F2** renames and
  **Delete** bins (Shift skips the confirmation). Only while no text field
  has focus; in a split the pane last clicked gets these keys.
- Transcript: Space play/pause, ←/→ skip (focus permitting).

## Checking a change visually

Debug builds can render any view off screen at any size:

```powershell
$env:WITNESS_SETTINGS = '<scratch data>\witness-settings.toml'  # auto_record = false
$env:WITNESS_DEBUG_START_VIEW = 'transcript:10'
$env:WITNESS_DEBUG_SIZE = '380x760'
$env:WITNESS_DEBUG_THEME = 'light'
$env:WITNESS_DEBUG_SNAPSHOT = 'C:\shots\transcript-380.bmp'
.\target\debug\witness.exe   # cloaked, never activated; writes the BMP and exits
```

The run is DWM-cloaked and never activated, takes no tray icon or hotkeys,
and uses per-process single-instance names, so it runs beside a real
instance. Check every change at **380, 460, 640, 1000 and 1400** wide (the
last shows the split) in both themes. Debug-only hooks reach states without
input:

- Shell: `WITNESS_DEBUG_FAKE_RECORDING=1|warn|busy`,
  `WITNESS_DEBUG_MODEL_BANNER=1`, `WITNESS_DEBUG_CONFIRM=1`,
  `WITNESS_DEBUG_TOASTS=1`, `WITNESS_DEBUG_PALETTE=<query>|-` (+
  `WITNESS_DEBUG_PALETTE_SELECT=<row>`), `WITNESS_DEBUG_ONBOARDING=<1-5>`
  (+ `WITNESS_DEBUG_ONBOARDING_FAKE=1`).
- Lists: `WITNESS_DEBUG_BIN=1`, `WITNESS_DEBUG_FILTERS=1`,
  `WITNESS_DEBUG_LOADING=1`, `WITNESS_DEBUG_EMPTY=1|bin`,
  `WITNESS_DEBUG_SELECT=<n>|bin:<n>`, `WITNESS_DEBUG_PANE=<index>`,
  `WITNESS_DEBUG_EXPAND_PERSON=<i>`.
- Settings: `WITNESS_DEBUG_SCROLL=<pt>`.
- Transcript: `WITNESS_DEBUG_SYNTHETIC=1`,
  `WITNESS_DEBUG_TRANSCRIPT_{NOTES,SEGMENT,HIGHLIGHT,MENU,VOLUME,RENAME}`,
  `WITNESS_DEBUG_TRANSCRIPT_FIND=<query>[,<hit>]`,
  `WITNESS_DEBUG_TRANSCRIPT_FILTER=<label>[,<label>]`,
  `WITNESS_DEBUG_TRANSCRIPT_LOADING=1`, `WITNESS_DEBUG_TRANSCRIPT_PANE=detail`.

The Mica backdrop and anything needing real input (hover, drag, typing)
can't be snapshotted — check those by hand.
