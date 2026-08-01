import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import {
  deleteBookmark,
  exportAudio,
  exportTranscript,
  getAudioUrl,
  getMeeting,
  getTranscriptText,
  renameMeeting,
  renameSpeaker,
  retranscribe,
  setBookmarkNote,
  setMeetingNotes,
  type Bookmark,
  type Engine,
  type MeetingDetail,
  type Segment,
  type TranscriptFormat,
} from "../lib/api";
import { fmtDate, fmtDuration } from "./meetings";
import type { LiveTranscript } from "../lib/events";
import { AudioPlayer } from "./player";
import {
  ArrowLeft,
  ArrowsClockwise,
  BookmarkSimple,
  CheckCircle,
  Export,
  PencilSimple,
  Trash,
} from "../lib/icons";
import { notifyError, notifyInfo, notifySuccess } from "../lib/notify";

function fmtTs(ms: number): string {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

// Speaker chip colors are assigned by label so they're stable across reloads.
const CHIP_CLASS: Record<string, string> = {
  me: "chip-me",
  S1: "chip-s1",
  S2: "chip-s2",
  S3: "chip-s3",
  S4: "chip-s4",
};

/** Case-insensitive term highlighting for search deep-links. */
function Highlighted({ text, terms }: { text: string; terms?: string }) {
  const tokens = (terms ?? "")
    .split(/\s+/)
    .map((t) => t.replace(/[^\p{L}\p{N}']/gu, ""))
    .filter((t) => t.length >= 2 && !["and", "or", "not", "near"].includes(t.toLowerCase()));
  if (tokens.length === 0) return <>{text}</>;
  const escaped = [...new Set(tokens)].map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const re = new RegExp(`(${escaped.join("|")})`, "gi");
  const parts = text.split(re);
  return (
    <>
      {parts.map((p, i) => (i % 2 === 1 ? <mark key={i}>{p}</mark> : p))}
    </>
  );
}

/** Binary search: index of the last segment with start_ms <= t, or -1. */
function segmentAt(segments: Segment[], t: number): number {
  let lo = 0,
    hi = segments.length - 1,
    ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (segments[mid].start_ms <= t) {
      ans = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return ans;
}

function handleMenuKey(
  event: KeyboardEvent,
  close: () => void,
  trigger: HTMLButtonElement | null,
) {
  const menu = event.currentTarget as HTMLElement;
  const items = [...menu.querySelectorAll<HTMLButtonElement>("button:not([disabled])")];
  const index = items.indexOf(document.activeElement as HTMLButtonElement);
  if (event.key === "Escape") {
    event.preventDefault();
    event.stopPropagation();
    close();
    trigger?.focus();
  } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const direction = event.key === "ArrowDown" ? 1 : -1;
    items[(index + direction + items.length) % items.length]?.focus();
  } else if (event.key === "Home") {
    event.preventDefault();
    items[0]?.focus();
  } else if (event.key === "End") {
    event.preventDefault();
    items.at(-1)?.focus();
  }
}

export function TranscriptView(props: {
  meetingId: number;
  focusSegmentId?: number;
  /** Search query to highlight inside segment text (deep-links). */
  highlight?: string;
  refreshTick: number;
  /** Provisional captions while this meeting is being recorded. */
  liveLines: LiveTranscript[] | null;
  onBack: () => void;
}) {
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [audioUrl, setAudioUrl] = useState<string | null>(null);
  const [audioError, setAudioError] = useState<string | null>(null);
  const [reloadTick, setReloadTick] = useState(0);
  const [activeIdx, setActiveIdx] = useState(-1);
  // Speaker rename: anchored to the specific segment whose chip was clicked.
  const [renaming, setRenaming] = useState<{ speakerId: number; segId: number } | null>(null);
  const [renameText, setRenameText] = useState("");
  const [engineMenu, setEngineMenu] = useState(false);
  const [exportMenu, setExportMenu] = useState(false);
  const [copied, setCopied] = useState(false);
  const [editingTitle, setEditingTitle] = useState(false);
  const [titleText, setTitleText] = useState("");
  const [notesDraft, setNotesDraft] = useState("");
  const [bmEditing, setBmEditing] = useState<{ id: number; text: string } | null>(null);
  const audioRef = useRef<HTMLAudioElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const focusedOnce = useRef(false);
  const notesMeetingId = useRef<number | null>(null);
  const copiedTimer = useRef<number | undefined>(undefined);
  const exportButtonRef = useRef<HTMLButtonElement>(null);
  const exportMenuRef = useRef<HTMLDivElement>(null);
  const engineButtonRef = useRef<HTMLButtonElement>(null);
  const engineMenuRef = useRef<HTMLDivElement>(null);
  const bookmarkCancel = useRef<number | null>(null);
  const speakerRenameCancel = useRef(false);

  useEffect(() => {
    let cancelled = false;
    setLoadError(null);
    setDetail((current) =>
      current?.meeting.id === props.meetingId ? current : null,
    );
    getMeeting(props.meetingId)
      .then((loaded) => {
        if (cancelled) return;
        setDetail(loaded);
        if (notesMeetingId.current !== props.meetingId) {
          notesMeetingId.current = props.meetingId;
          setNotesDraft(loaded.meeting.notes);
        }
      })
      .catch((error) => {
        if (cancelled) return;
        setDetail(null);
        setLoadError(String(error));
        notifyError("Could not load the meeting", error);
      });
    return () => {
      cancelled = true;
    };
  }, [props.meetingId, props.refreshTick, reloadTick]);

  useEffect(() => {
    let cancelled = false;
    setAudioUrl(null);
    setAudioError(null);
    getAudioUrl(props.meetingId)
      .then((url) => {
        if (!cancelled) setAudioUrl(url);
      })
      .catch((error) => {
        if (!cancelled) {
          setAudioUrl(null);
          setAudioError(String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [props.meetingId]);

  useEffect(
    () => () => {
      window.clearTimeout(copiedTimer.current);
    },
    [],
  );

  useEffect(() => {
    if (!exportMenu && !engineMenu) return;
    const menu = exportMenu ? exportMenuRef.current : engineMenuRef.current;
    requestAnimationFrame(() => menu?.querySelector<HTMLButtonElement>("button")?.focus());

    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node;
      if (
        !exportMenuRef.current?.contains(target) &&
        !engineMenuRef.current?.contains(target) &&
        !exportButtonRef.current?.contains(target) &&
        !engineButtonRef.current?.contains(target)
      ) {
        setExportMenu(false);
        setEngineMenu(false);
      }
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setExportMenu(false);
      setEngineMenu(false);
      (exportMenu ? exportButtonRef.current : engineButtonRef.current)?.focus();
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [exportMenu, engineMenu]);

  const saveNotes = () => {
    if (detail && notesDraft !== detail.meeting.notes) {
      setMeetingNotes(props.meetingId, notesDraft)
        .then(() =>
          setDetail((d) =>
            d?.meeting.id === props.meetingId
              ? { ...d, meeting: { ...d.meeting, notes: notesDraft } }
              : d,
          ),
        )
        .catch((error) => notifyError("Could not save notes", error));
    }
  };

  const speakersById = useMemo(() => {
    const m = new Map<
      number,
      { label: string; display_name: string; auto_labeled: boolean }
    >();
    for (const s of detail?.speakers ?? []) m.set(s.id, s);
    return m;
  }, [detail]);

  const segments = detail?.segments ?? [];
  const bookmarks = detail?.bookmarks ?? [];

  // Bookmarks interleave with segments by time.
  type Row =
    | { kind: "seg"; seg: Segment; idx: number; at: number }
    | { kind: "bm"; bm: Bookmark; at: number };
  const rows = useMemo<Row[]>(() => {
    const rs: Row[] = segments.map((seg, idx) => ({
      kind: "seg" as const,
      seg,
      idx,
      at: seg.start_ms,
    }));
    for (const bm of bookmarks) rs.push({ kind: "bm", bm, at: bm.at_ms });
    rs.sort((a, b) => a.at - b.at);
    return rs;
  }, [segments, bookmarks]);

  // Per-speaker share for the stats bar (finished transcripts only).
  const speakerStats = useMemo(() => {
    if (segments.length === 0) return [];
    const acc = new Map<string, { name: string; label: string; talk: number; words: number }>();
    for (const seg of segments) {
      const sp = seg.speaker_id != null ? speakersById.get(seg.speaker_id) : undefined;
      const label = sp?.label ?? (seg.track === "mic" ? "me" : "S?");
      const key = String(seg.speaker_id ?? label);
      const entry = acc.get(key) ?? {
        name: sp?.display_name ?? label,
        label,
        talk: 0,
        words: 0,
      };
      entry.talk += seg.end_ms - seg.start_ms;
      entry.words += seg.text.split(/\s+/).filter(Boolean).length;
      acc.set(key, entry);
    }
    const list = [...acc.values()].sort((a, b) => b.talk - a.talk);
    const total = list.reduce((s, x) => s + x.talk, 0) || 1;
    return list.map((x) => ({ ...x, share: (x.talk / total) * 100 }));
  }, [segments, speakersById]);

  // Deep-link from search: scroll to the target segment once loaded.
  useEffect(() => {
    focusedOnce.current = false;
    setActiveIdx(-1);
  }, [props.meetingId, props.focusSegmentId]);

  useEffect(() => {
    if (focusedOnce.current || !props.focusSegmentId || segments.length === 0) return;
    const idx = segments.findIndex((s) => s.id === props.focusSegmentId);
    if (idx >= 0) {
      focusedOnce.current = true;
      setActiveIdx(idx);
      requestAnimationFrame(() => {
        const target = listRef.current?.querySelector<HTMLElement>(
          `[data-idx="${idx}"]`,
        );
        target?.focus({ preventScroll: true });
        target?.scrollIntoView({ block: "center" });
      });
    }
  }, [segments, props.focusSegmentId]);

  const onTimeUpdate = () => {
    const a = audioRef.current;
    if (!a) return;
    const idx = segmentAt(segments, a.currentTime * 1000);
    if (idx !== activeIdx) {
      setActiveIdx(idx);
      if (idx >= 0) {
        listRef.current
          ?.querySelector(`[data-idx="${idx}"]`)
          ?.scrollIntoView({ block: "nearest", behavior: "smooth" });
      }
    }
  };

  const seekTo = (seg: Segment, idx: number) => {
    const a = audioRef.current;
    if (!a) return;
    a.currentTime = seg.start_ms / 1000;
    a.play().catch((error) => notifyError("Could not play the recording", error));
    setActiveIdx(idx);
  };

  const commitSpeakerRename = (speakerId: number) => {
    const name = renameText.trim();
    setRenaming(null);
    if (!name) return;
    const currentSpeaker = detail?.speakers.find((speaker) => speaker.id === speakerId);
    if (name === currentSpeaker?.display_name.trim() && !currentSpeaker.auto_labeled) return;
    renameSpeaker(speakerId, name)
      .then(() =>
        setDetail((d) =>
          d?.meeting.id === props.meetingId
            ? {
                ...d,
                speakers: d.speakers.map((s) =>
                  s.id === speakerId
                    ? { ...s, display_name: name, auto_labeled: false }
                    : s,
                ),
              }
            : d,
        ),
      )
      .catch((error) => notifyError("Could not rename the speaker", error));
  };

  const doRetranscribe = (engine?: Engine) => {
    setEngineMenu(false);
    retranscribe(props.meetingId, engine)
      .then(() => notifyInfo("Transcription queued"))
      .catch((error) => notifyError("Could not queue transcription", error));
  };

  const copyTranscript = async () => {
    setExportMenu(false);
    try {
      const text = await getTranscriptText(props.meetingId, "txt");
      try {
        await navigator.clipboard.writeText(text);
      } catch {
        // Clipboard API can be finicky in webviews — textarea fallback.
        const ta = document.createElement("textarea");
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand("copy");
        ta.remove();
      }
      setCopied(true);
      notifySuccess("Transcript copied to the clipboard");
      window.clearTimeout(copiedTimer.current);
      copiedTimer.current = window.setTimeout(() => setCopied(false), 1500);
    } catch (error) {
      notifyError("Could not copy the transcript", error);
    }
  };

  const doExport = (format: TranscriptFormat) => {
    setExportMenu(false);
    exportTranscript(props.meetingId, format).catch((error) =>
      notifyError("Could not export the transcript", error),
    );
  };

  if (loadError) {
    return (
      <div class="view-error" role="alert">
        <p>Could not load meeting: {loadError}</p>
        <div class="view-error-actions">
          <button type="button" class="btn btn-ghost" onClick={props.onBack}>
            Back
          </button>
          <button type="button" class="btn" onClick={() => setReloadTick((tick) => tick + 1)}>
            Try again
          </button>
        </div>
      </div>
    );
  }
  if (!detail) return <div class="empty" role="status">Loading…</div>;
  const m = detail.meeting;

  // Live captions take over while the meeting is still recording (the final
  // pipeline pass replaces them with the diarized transcript).
  const showLive =
    props.liveLines != null && segments.length === 0 && m.status === "recording";
  const liveSorted = showLive
    ? [...props.liveLines!].sort((a, b) => a.start_ms - b.start_ms)
    : [];

  return (
    <div class="transcript-view">
      <div class="transcript-header">
        <button type="button" class="btn btn-ghost btn-with-icon" title="Back" aria-label="Back" onClick={props.onBack}>
          <ArrowLeft size={16} />
        </button>
        <div class="transcript-titleblock">
          {editingTitle ? (
            <input
              class="rename-input title-input"
              value={titleText}
              autoFocus
              onInput={(e) => setTitleText((e.target as HTMLInputElement).value)}
              onBlur={() => {
                setEditingTitle(false);
                const title = titleText.trim();
                if (title && title !== m.title) {
                  renameMeeting(m.id, title)
                    .then(() =>
                      setDetail((d) =>
                        d?.meeting.id === props.meetingId
                          ? { ...d, meeting: { ...d.meeting, title } }
                          : d,
                      ),
                    )
                    .catch((error) => notifyError("Could not rename the meeting", error));
                }
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                if (e.key === "Escape") setEditingTitle(false);
              }}
            />
          ) : (
            <h2
              title="Double-click to rename"
              onDblClick={() => {
                setTitleText(m.title);
                setEditingTitle(true);
              }}
            >
              {m.title}
              <button
                type="button"
                class="icon-btn rename-btn"
                title="Rename meeting"
                aria-label="Rename meeting"
                onClick={() => {
                  setTitleText(m.title);
                  setEditingTitle(true);
                }}
              >
                <PencilSimple size={14} />
              </button>
            </h2>
          )}
          <span class="meeting-meta">
            {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)}
            {m.engine ? ` · ${m.engine}` : ""}
          </span>
        </div>
        <div class="menu-wrap">
          <button
            type="button"
            ref={exportButtonRef}
            class="btn btn-ghost btn-with-icon"
            title={copied ? "Copied to clipboard" : "Export transcript"}
            aria-label="Export transcript"
            aria-haspopup="menu"
            aria-expanded={exportMenu}
            aria-controls="transcript-export-menu"
            onClick={() => { setExportMenu(!exportMenu); setEngineMenu(false); }}
          >
            {copied ? <CheckCircle size={16} /> : <Export size={16} />}
          </button>
          {exportMenu && (
            <div
              class="menu"
              id="transcript-export-menu"
              ref={exportMenuRef}
              role="menu"
              onKeyDown={(event) =>
                handleMenuKey(event, () => setExportMenu(false), exportButtonRef.current)
              }
            >
              <button type="button" role="menuitem" onClick={copyTranscript}>Copy to clipboard</button>
              <button type="button" role="menuitem" onClick={() => doExport("md")}>Markdown…</button>
              <button type="button" role="menuitem" onClick={() => doExport("txt")}>Plain text…</button>
              <button type="button" role="menuitem" onClick={() => doExport("srt")}>Subtitles (SRT)…</button>
              <button type="button" role="menuitem" onClick={() => doExport("vtt")}>Subtitles (WebVTT)…</button>
            </div>
          )}
        </div>
        <div class="menu-wrap">
          <button
            type="button"
            ref={engineButtonRef}
            class="btn btn-ghost btn-with-icon"
            title="Retranscribe"
            aria-label="Retranscribe"
            aria-haspopup="menu"
            aria-expanded={engineMenu}
            aria-controls="transcript-engine-menu"
            onClick={() => { setEngineMenu(!engineMenu); setExportMenu(false); }}
          >
            <ArrowsClockwise size={16} />
          </button>
          {engineMenu && (
            <div
              class="menu"
              id="transcript-engine-menu"
              ref={engineMenuRef}
              role="menu"
              onKeyDown={(event) =>
                handleMenuKey(event, () => setEngineMenu(false), engineButtonRef.current)
              }
            >
              <button type="button" role="menuitem" onClick={() => doRetranscribe("parakeet")}>with Parakeet</button>
              <button type="button" role="menuitem" onClick={() => doRetranscribe("whisper")}>with Whisper</button>
            </div>
          )}
        </div>
      </div>

      {speakerStats.length > 0 && (
        <div class="stats-bar" aria-label="Speaker statistics">
          {speakerStats.map((s) => (
            <span class="stat-chip" key={s.name}>
              <span class={`chip ${CHIP_CLASS[s.label] ?? "chip-s1"}`}>{s.name}</span>
              {fmtDuration(s.talk)} · {s.words.toLocaleString()} words ·{" "}
              {Math.round(s.share)}%
            </span>
          ))}
        </div>
      )}

      {showLive && (
        <div class="live-feed" role="log" aria-live="polite" aria-relevant="additions">
          <div class="live-header">
            <span class="rec-dot" /> Live captions
            <span class="muted">· provisional</span>
          </div>
          {liveSorted.length === 0 && (
            <p class="muted">Listening… captions appear a few seconds after speech.</p>
          )}
          {liveSorted.map((l) => (
            <div class="segment" key={`${l.track}-${l.start_ms}`}>
              <span class={`chip ${l.track === "mic" ? "chip-me" : "chip-s1"}`}>
                {l.track === "mic" ? "Me" : "Them"}
              </span>
              <span class="ts">[{fmtTs(l.start_ms)}]</span>
              <span class="segment-text">{l.text}</span>
            </div>
          ))}
          <div ref={(el) => el?.scrollIntoView({ block: "nearest" })} />
        </div>
      )}

      <div class="notes-panel">
        <details open={!!(detail.meeting.notes || notesDraft)}>
          <summary class="muted">Notes</summary>
          <textarea
            class="notes-input"
            aria-label="Meeting notes"
            placeholder="Meeting notes… (searchable)"
            value={notesDraft}
            onInput={(e) => setNotesDraft((e.target as HTMLTextAreaElement).value)}
            onBlur={saveNotes}
          />
        </details>
      </div>

      {audioError && m.audio_path && (
        <div class="inline-warning" role="status">
          Audio playback is unavailable: {audioError}
        </div>
      )}

      <div class="segments" ref={listRef} aria-label="Transcript">
        {segments.length === 0 && !showLive && bookmarks.length === 0 && (
          <div class="empty">
            <p class="muted">
              {m.status === "recorded"
                ? "Not transcribed yet."
                : m.status === "processing"
                  ? "Transcription in progress…"
                  : m.status === "failed"
                    ? "Transcription failed — use Retranscribe to retry."
                    : "No transcript."}
            </p>
          </div>
        )}
        {rows.map((row) => {
          if (row.kind === "bm") {
            const bm = row.bm;
            return (
              <div
                class="segment bookmark-row"
                key={`bm${bm.id}`}
              >
                <button
                  type="button"
                  class="bookmark-seek"
                  aria-label={`Play recording at bookmark ${fmtTs(bm.at_ms)}`}
                  aria-disabled={!audioUrl}
                  onClick={() => {
                  const a = audioRef.current;
                  if (a) {
                    a.currentTime = bm.at_ms / 1000;
                    a.play().catch((error) =>
                      notifyError("Could not play the recording", error),
                    );
                  }
                }}
                >
                  <span class="chip chip-bookmark" aria-hidden="true">
                    <BookmarkSimple size={11} />
                  </span>
                  <span class="ts">[{fmtTs(bm.at_ms)}]</span>
                </button>
                {bmEditing?.id === bm.id ? (
                  <input
                    class="rename-input"
                    value={bmEditing.text}
                    autoFocus
                    aria-label="Bookmark note"
                    placeholder="what happened here?"
                    onInput={(e) =>
                      setBmEditing({ id: bm.id, text: (e.target as HTMLInputElement).value })
                    }
                    onBlur={() => {
                      if (bookmarkCancel.current === bm.id) {
                        bookmarkCancel.current = null;
                        return;
                      }
                      const note = bmEditing.text.trim();
                      setBmEditing(null);
                      setBookmarkNote(bm.id, note)
                        .then(() =>
                          setDetail((d) =>
                            d?.meeting.id === props.meetingId
                              ? {
                                  ...d,
                                  bookmarks: d.bookmarks.map((x) =>
                                    x.id === bm.id ? { ...x, note } : x,
                                  ),
                                }
                              : d,
                          ),
                        )
                        .catch((error) => notifyError("Could not save the bookmark", error));
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                      if (e.key === "Escape") {
                        bookmarkCancel.current = bm.id;
                        setBmEditing(null);
                      }
                    }}
                  />
                ) : (
                  <button
                    type="button"
                    class={`bookmark-note segment-text ${bm.note ? "" : "muted"}`}
                    title="Click to edit note"
                    onClick={() => {
                      bookmarkCancel.current = null;
                      setBmEditing({ id: bm.id, text: bm.note });
                    }}
                  >
                    {bm.note || "bookmark — click to add a note"}
                  </button>
                )}
                <button
                  type="button"
                  class="icon-btn"
                  title="Remove bookmark"
                  aria-label={`Remove bookmark at ${fmtTs(bm.at_ms)}`}
                  onClick={() => {
                    deleteBookmark(bm.id)
                      .then(() =>
                        setDetail((d) =>
                          d?.meeting.id === props.meetingId
                            ? { ...d, bookmarks: d.bookmarks.filter((x) => x.id !== bm.id) }
                            : d,
                        ),
                      )
                      .catch((error) => notifyError("Could not remove the bookmark", error));
                  }}
                >
                  <Trash size={14} />
                </button>
              </div>
            );
          }
          const { seg, idx } = row;
          const sp = seg.speaker_id != null ? speakersById.get(seg.speaker_id) : undefined;
          const label = sp?.label ?? (seg.track === "mic" ? "me" : "S?");
          return (
            <div
              class={`segment ${idx === activeIdx ? "segment-active" : ""}`}
              key={seg.id}
            >
              <button
                type="button"
                class={`chip ${CHIP_CLASS[label] ?? "chip-s1"}`}
                title={
                  sp?.auto_labeled
                    ? "Auto-matched by voice — click to rename/confirm"
                    : "Click to rename speaker"
                }
                disabled={seg.speaker_id == null}
                onClick={() => {
                  if (seg.speaker_id != null) {
                    speakerRenameCancel.current = false;
                    setRenaming({ speakerId: seg.speaker_id, segId: seg.id });
                    setRenameText(sp?.display_name ?? "");
                  }
                }}
              >
                {sp?.auto_labeled ? "≈ " : ""}
                {sp?.display_name ?? label}
              </button>
              {renaming != null && renaming.segId === seg.id && (
                <input
                  class="rename-input"
                  value={renameText}
                  autoFocus
                  aria-label={`Rename ${sp?.display_name ?? label}`}
                  onInput={(e) => setRenameText((e.target as HTMLInputElement).value)}
                  onBlur={() => {
                    if (speakerRenameCancel.current) {
                      speakerRenameCancel.current = false;
                    } else {
                      commitSpeakerRename(renaming.speakerId);
                    }
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitSpeakerRename(renaming.speakerId);
                    if (e.key === "Escape") {
                      speakerRenameCancel.current = true;
                      setRenaming(null);
                    }
                  }}
                />
              )}
              <button
                type="button"
                class="segment-seek"
                data-idx={idx}
                aria-current={idx === activeIdx ? "true" : undefined}
                aria-label={`Play at ${fmtTs(seg.start_ms)}: ${seg.text}`}
                aria-disabled={!audioUrl}
                onClick={() => seekTo(seg, idx)}
              >
                <span class="ts">[{fmtTs(seg.start_ms)}]</span>
                <span class="segment-text">
                  <Highlighted text={seg.text} terms={props.highlight} />
                </span>
              </button>
            </div>
          );
        })}
      </div>

      {audioUrl && (
        <div class="player-bar">
          <AudioPlayer
            src={audioUrl}
            audioRef={audioRef}
            markers={bookmarks.map((b) => b.at_ms)}
            onTimeUpdate={onTimeUpdate}
            onDownload={() =>
              exportAudio(m.id).catch((error) =>
                notifyError("Could not export the audio", error),
              )
            }
          />
        </div>
      )}
    </div>
  );
}
