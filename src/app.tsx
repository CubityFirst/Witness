import { Component, type ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import {
  type AppStatus,
  bookmarkNow,
  downloadModels,
  getModelStatus,
  getStatus,
  startRecording,
  stopRecording,
} from "./lib/api";
import {
  onLiveTranscript,
  onMeetingsChanged,
  onModelDownloadProgress,
  onRecordingLevel,
  onRecordingHealth,
  onRecordingStarted,
  onRecordingStopped,
  onTranscriptionComplete,
  onTranscriptionFailed,
  onTranscriptionProgress,
  onTrayPopup,
  onWatcherStatus,
  type LiveTranscript,
  type ModelDownloadProgress,
  type RecordingLevel,
  type RecordingHealth,
  type TranscriptionProgress,
} from "./lib/events";
import { MeetingsView } from "./views/meetings";
import { TranscriptView } from "./views/transcript";
import { SearchView } from "./views/search";
import { SettingsView } from "./views/settings";
import { PeopleView } from "./views/people";
import { ConfirmHost } from "./lib/confirm";
import {
  NotificationHost,
  notifyError,
  notifySuccess,
} from "./lib/notify";
import { BookmarkSimple, GearSix, Minus, Record, Square, Stop, X } from "./lib/icons";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** True when the event landed on an interactive element (skip window drag). */
function onInteractive(e: MouseEvent): boolean {
  const t = e.target as HTMLElement | null;
  return !!t?.closest("button, input, select, a, textarea, .pill-click");
}

/**
 * Presses within this many pixels of a window edge belong to the native
 * resize borders (undecorated windows still hit-test them) — the title-bar
 * drag must not swallow those, or the window can't be resized from the top.
 */
function onResizeBorder(e: MouseEvent): boolean {
  const m = 8;
  return (
    e.clientY < m ||
    e.clientX < m ||
    window.innerWidth - e.clientX < m
  );
}

export type View =
  | { kind: "meetings" }
  | { kind: "people" }
  | { kind: "transcript"; meetingId: number; segmentId?: number; highlight?: string }
  | { kind: "search"; query: string }
  | { kind: "settings" };

class ViewErrorBoundary extends Component<
  { children: ComponentChildren },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error) {
    notifyError("This view could not be displayed", error);
  }

  render() {
    if (this.state.error) {
      return (
        <div class="view-error" role="alert">
          <h2>Something went wrong</h2>
          <p>The current view could not be displayed.</p>
          <button
            type="button"
            class="btn"
            onClick={() => this.setState({ error: null })}
          >
            Try again
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

function fmtElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(sec).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

function captureWarning(health: RecordingHealth | null): string | null {
  if (!health) return null;
  if (health.writer_error) return `Audio writer failed: ${health.writer_error}`;
  for (const [name, track] of [
    ["Microphone", health.mic],
    ["System audio", health.loopback],
  ] as const) {
    if (track.fatal_error) return `${name} capture failed: ${track.fatal_error}`;
    if (track.device_loss_count > 0) return `${name} device was lost during recording`;
    if (track.capture_overflow_count > 0)
      return `${name} capture dropped ${track.capture_dropped_ms} ms of audio`;
    if (track.live_dropped_ms > 0)
      return `Live captions skipped ${track.live_dropped_ms} ms to keep recording responsive`;
    if (track.live_disconnected) return `Live captions stopped receiving ${name.toLowerCase()}`;
  }
  return null;
}

export function App() {
  const [view, setView] = useState<View>({ kind: "meetings" });
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [level, setLevel] = useState<RecordingLevel | null>(null);
  const [health, setHealth] = useState<RecordingHealth | null>(null);
  const [progress, setProgress] = useState<TranscriptionProgress | null>(null);
  const [searchText, setSearchText] = useState("");
  const [refreshTick, setRefreshTick] = useState(0);
  // Provisional captions for the meeting currently being recorded.
  const [liveLines, setLiveLines] = useState<LiveTranscript[]>([]);
  const [liveMeetingId, setLiveMeetingId] = useState<number | null>(null);
  // First-run: prompt to fetch the transcription models.
  const [modelsMissing, setModelsMissing] = useState(false);
  const [modelDl, setModelDl] = useState<ModelDownloadProgress | null>(null);
  const [recordingAction, setRecordingAction] = useState(false);
  // Tray-flyout mode: no window controls (blur dismisses the window).
  const [popupMode, setPopupMode] = useState(false);
  const searchDebounce = useRef<number | undefined>(undefined);
  const contentRef = useRef<HTMLElement>(null);

  const refreshStatus = () =>
    getStatus()
      .then(setStatus)
      .catch((error) => notifyError("Could not refresh application status", error));

  useEffect(() => {
    refreshStatus();
    // First-run banner: transcription models not fetched yet.
    if (localStorage.getItem("witness-model-banner-dismissed") !== "1") {
      getModelStatus()
        .then((models) =>
          setModelsMissing(models.some((m) => m.engine === "parakeet" && !m.present)),
        )
        .catch((error) => notifyError("Could not check transcription models", error));
    }
    const unlisteners = [
      onTrayPopup(setPopupMode),
      onModelDownloadProgress((p) => {
        setModelDl(p.done && !p.error ? null : p);
        if (p.done && p.error) notifyError("Model download failed", p.error);
        if (p.done && !p.error) {
          getModelStatus()
            .then((models) =>
              setModelsMissing(models.some((m) => m.engine === "parakeet" && !m.present)),
            )
            .catch((error) =>
              notifyError("Could not refresh transcription models", error),
            );
        }
      }),
      onRecordingStarted((e) => {
        setHealth(null);
        setLiveLines([]);
        setLiveMeetingId(e.meeting_id);
        refreshStatus();
      }),
      onLiveTranscript((line) =>
        setLiveLines((prev) => [...prev.slice(-499), line]),
      ),
      onRecordingStopped(() => {
        setLevel(null);
        setHealth(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
      }),
      onRecordingLevel(setLevel),
      onRecordingHealth(setHealth),
      onWatcherStatus((w) =>
        setStatus((s) => (s ? { ...s, watcher: w } : s)),
      ),
      onTranscriptionProgress(setProgress),
      onTranscriptionComplete(() => {
        setProgress(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
      }),
      onTranscriptionFailed((event) => {
        setProgress(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
        notifyError("Transcription failed", event.error ?? "Unknown error");
      }),
      onMeetingsChanged(() => setRefreshTick((t) => t + 1)),
    ].map((listener) =>
      listener.catch((error) => {
        notifyError("Could not connect to application events", error);
        return () => {};
      }),
    );
    return () => {
      for (const p of unlisteners) {
        p.then((un) => un()).catch((error) =>
          notifyError("Could not detach an application event", error),
        );
      }
      window.clearTimeout(searchDebounce.current);
    };
  }, []);

  const recording = status?.recording ?? false;
  const healthWarning = captureWarning(health);
  const viewKey =
    view.kind === "transcript" ? `transcript-${view.meetingId}` : view.kind;

  useEffect(() => {
    const active = document.activeElement;
    if (!active || !document.contains(active)) contentRef.current?.focus();
  }, [viewKey]);

  const toggleRecording = async () => {
    if (recordingAction) return;
    setRecordingAction(true);
    try {
      if (recording) await stopRecording();
      else await startRecording();
      await refreshStatus();
    } catch (error) {
      notifyError(
        recording ? "Could not stop recording" : "Could not start recording",
        error,
      );
    } finally {
      setRecordingAction(false);
    }
  };

  const onSearchInput = (q: string) => {
    setSearchText(q);
    window.clearTimeout(searchDebounce.current);
    searchDebounce.current = window.setTimeout(() => {
      if (q.trim()) setView({ kind: "search", query: q });
      else if (view.kind === "search") setView({ kind: "meetings" });
    }, 250);
  };

  let statusPill;
  if (recording) {
    statusPill = (
      <button
        type="button"
        class="pill pill-recording pill-click"
        title={healthWarning ?? "Open live captions"}
        aria-label={`${fmtElapsed(level?.elapsed_ms ?? 0)} recording. ${
          healthWarning ?? "Open live transcript"
        }`}
        onClick={() =>
          status?.meeting_id != null &&
          setView({ kind: "transcript", meetingId: status.meeting_id })
        }
      >
        <span class="rec-dot" /> Recording {fmtElapsed(level?.elapsed_ms ?? 0)}
        {healthWarning && <span aria-hidden="true"> ⚠</span>}
        <span
          class="level-meter"
          title="Microphone and system audio levels"
          aria-hidden="true"
        >
          <span
            class="level-bar level-mic"
            style={{ width: `${Math.min(100, (level?.mic_rms ?? 0) * 300)}%` }}
          />
          <span
            class="level-bar level-loop"
            style={{ width: `${Math.min(100, (level?.loopback_rms ?? 0) * 300)}%` }}
          />
        </span>
      </button>
    );
  } else if (progress) {
    statusPill = (
      <span class="pill pill-busy" role="status" aria-live="polite">
        Transcribing ({progress.stage}) {Math.round(progress.pct)}%
      </span>
    );
  } else if (status?.transcribing_meeting_id != null) {
    // App opened mid-job: retain the durable progress snapshot from the backend.
    const persistedProgress =
      status.processing_stage == null
        ? "Transcribing…"
        : `Transcribing (${status.processing_stage})${
            status.processing_pct == null ? "" : ` ${Math.round(status.processing_pct)}%`
          }`;
    statusPill = (
      <span class="pill pill-busy" role="status">
        {persistedProgress}
      </span>
    );
  } else {
    statusPill = <span class="pill pill-idle" role="status">Idle</span>;
  }

  return (
    <div class="app">
      <header
        class="topbar"
        // The top bar doubles as the (chrome-less) window title bar.
        onMouseDown={(e) => {
          if (e.buttons === 1 && !onInteractive(e) && !onResizeBorder(e)) {
            getCurrentWindow()
              .startDragging()
              .catch((error) => notifyError("Could not move the window", error));
          }
        }}
        onDblClick={(e) => {
          if (!onInteractive(e)) {
            getCurrentWindow()
              .toggleMaximize()
              .catch((error) => notifyError("Could not resize the window", error));
          }
        }}
      >
        <button
          type="button"
          class={recording ? "btn btn-with-icon btn-stop" : "btn btn-with-icon btn-record"}
          title={recording ? "Stop recording" : "Start recording"}
          aria-label={recording ? "Stop recording" : "Start recording"}
          aria-busy={recordingAction}
          disabled={recordingAction}
          onClick={toggleRecording}
        >
          {recording ? <Stop size={17} /> : <Record size={17} />}
        </button>
        {recording && (
          <button
            type="button"
            class="btn btn-with-icon"
            title="Bookmark this moment (Ctrl+Alt+B)"
            aria-label="Bookmark this moment"
            onClick={() =>
              bookmarkNow()
                .then(() => notifySuccess("Bookmark added"))
                .catch((error) => notifyError("Could not add bookmark", error))
            }
          >
            <BookmarkSimple size={16} />
          </button>
        )}
        {statusPill}
        <nav class="nav" aria-label="Primary navigation">
          <button
            type="button"
            class={view.kind === "meetings" ? "nav-btn active" : "nav-btn"}
            aria-current={view.kind === "meetings" ? "page" : undefined}
            onClick={() => setView({ kind: "meetings" })}
          >
            Meetings
          </button>
          <button
            type="button"
            class={view.kind === "people" ? "nav-btn active" : "nav-btn"}
            aria-current={view.kind === "people" ? "page" : undefined}
            onClick={() => setView({ kind: "people" })}
          >
            People
          </button>
          <button
            type="button"
            class={view.kind === "settings" ? "nav-btn nav-btn-icon active" : "nav-btn nav-btn-icon"}
            title="Settings"
            aria-label="Settings"
            aria-current={view.kind === "settings" ? "page" : undefined}
            onClick={() => setView({ kind: "settings" })}
          >
            <GearSix />
          </button>
        </nav>
        {!popupMode && (
        <div class="window-controls">
          <button
            type="button"
            class="win-btn"
            title="Minimize"
            aria-label="Minimize"
            onClick={() =>
              getCurrentWindow()
                .minimize()
                .catch((error) => notifyError("Could not minimize the window", error))
            }
          >
            <Minus size={14} />
          </button>
          <button
            type="button"
            class="win-btn"
            title="Maximize"
            aria-label="Maximize"
            onClick={() =>
              getCurrentWindow()
                .toggleMaximize()
                .catch((error) => notifyError("Could not resize the window", error))
            }
          >
            <Square size={12} />
          </button>
          <button
            type="button"
            class="win-btn win-close"
            title="Close to tray"
            aria-label="Close to tray"
            onClick={() =>
              getCurrentWindow()
                .hide()
                .catch((error) => notifyError("Could not hide the window", error))
            }
          >
            <X size={14} />
          </button>
        </div>
        )}
      </header>
      <main class="content" ref={contentRef} tabIndex={-1}>
        {modelsMissing && (
          <div class="model-banner" role="status">
            <span>
              <b>Transcription models not installed.</b> Recording works, but
              nothing gets transcribed. One-time download, ~3.5 GB (Parakeet +
              speaker models).
            </span>
            {modelDl && !modelDl.error ? (
              <span class="muted" aria-live="polite">
                {modelDl.total_bytes
                  ? `${Math.round((modelDl.downloaded_bytes / modelDl.total_bytes) * 100)}% of ${modelDl.file}`
                  : "downloading…"}
              </span>
            ) : (
              <button
                type="button"
                class="btn"
                onClick={() =>
                  downloadModels("parakeet").catch((error) =>
                    notifyError("Could not start model download", error),
                  )
                }
              >
                Download now
              </button>
            )}
            <button
              type="button"
              class="btn btn-ghost"
              onClick={() => {
                localStorage.setItem("witness-model-banner-dismissed", "1");
                setModelsMissing(false);
              }}
            >
              Later
            </button>
          </div>
        )}
        {(view.kind === "meetings" || view.kind === "search") && (
          <div class="list-toolbar">
            <input
              class="search-input"
              type="search"
              aria-label="Search meeting transcripts"
              placeholder="Search transcripts…"
              value={searchText}
              onInput={(e) => onSearchInput((e.target as HTMLInputElement).value)}
            />
          </div>
        )}
        <ViewErrorBoundary key={viewKey}>
        {view.kind === "meetings" && (
          <MeetingsView
            refreshTick={refreshTick}
            onOpen={(meetingId) => setView({ kind: "transcript", meetingId })}
          />
        )}
        {view.kind === "people" && (
          <PeopleView
            refreshTick={refreshTick}
            onOpen={(meetingId) => setView({ kind: "transcript", meetingId })}
          />
        )}
        {view.kind === "transcript" && (
          <TranscriptView
            meetingId={view.meetingId}
            focusSegmentId={view.segmentId}
            highlight={view.highlight}
            refreshTick={refreshTick}
            liveLines={
              // Fall back to get_status so a page reload mid-recording
              // (dev hot-reload, reopened window) still shows the live bar.
              view.meetingId ===
              (liveMeetingId ?? (status?.recording ? status.meeting_id : null))
                ? liveLines
                : null
            }
            onBack={() =>
              // Return to the search results if a search is still active.
              setView(
                searchText.trim()
                  ? { kind: "search", query: searchText }
                  : { kind: "meetings" },
              )
            }
          />
        )}
        {view.kind === "search" && (
          <SearchView
            query={view.query}
            onOpen={(meetingId, segmentId) =>
              setView({ kind: "transcript", meetingId, segmentId, highlight: view.query })
            }
          />
        )}
        {view.kind === "settings" && <SettingsView watcher={status?.watcher ?? null} />}
        </ViewErrorBoundary>
      </main>
      <ConfirmHost />
      <NotificationHost />
    </div>
  );
}
