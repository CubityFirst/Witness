import { useEffect, useRef, useState } from "preact/hooks";
import {
  type AppStatus,
  getStatus,
  startRecording,
  stopRecording,
} from "./lib/api";
import {
  onLiveTranscript,
  onMeetingsChanged,
  onRecordingLevel,
  onRecordingStarted,
  onRecordingStopped,
  onTranscriptionComplete,
  onTranscriptionFailed,
  onTranscriptionProgress,
  onWatcherStatus,
  type LiveTranscript,
  type RecordingLevel,
  type TranscriptionProgress,
} from "./lib/events";
import { MeetingsView } from "./views/meetings";
import { TranscriptView } from "./views/transcript";
import { SearchView } from "./views/search";
import { SettingsView } from "./views/settings";
import { PeopleView } from "./views/people";
import { GearSix, Record, Stop } from "./lib/icons";

export type View =
  | { kind: "meetings" }
  | { kind: "people" }
  | { kind: "transcript"; meetingId: number; segmentId?: number }
  | { kind: "search"; query: string }
  | { kind: "settings" };

function fmtElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(sec).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

export function App() {
  const [view, setView] = useState<View>({ kind: "meetings" });
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [level, setLevel] = useState<RecordingLevel | null>(null);
  const [progress, setProgress] = useState<TranscriptionProgress | null>(null);
  const [searchText, setSearchText] = useState("");
  const [refreshTick, setRefreshTick] = useState(0);
  // Provisional captions for the meeting currently being recorded.
  const [liveLines, setLiveLines] = useState<LiveTranscript[]>([]);
  const [liveMeetingId, setLiveMeetingId] = useState<number | null>(null);
  const searchDebounce = useRef<number | undefined>(undefined);

  const refreshStatus = () => getStatus().then(setStatus).catch(() => {});

  useEffect(() => {
    refreshStatus();
    const unlisteners = [
      onRecordingStarted((e) => {
        setLiveLines([]);
        setLiveMeetingId(e.meeting_id);
        refreshStatus();
      }),
      onLiveTranscript((line) =>
        setLiveLines((prev) => [...prev.slice(-499), line]),
      ),
      onRecordingStopped(() => {
        setLevel(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
      }),
      onRecordingLevel(setLevel),
      onWatcherStatus((w) =>
        setStatus((s) => (s ? { ...s, watcher: w } : s)),
      ),
      onTranscriptionProgress(setProgress),
      onTranscriptionComplete(() => {
        setProgress(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
      }),
      onTranscriptionFailed(() => {
        setProgress(null);
        refreshStatus();
        setRefreshTick((t) => t + 1);
      }),
      onMeetingsChanged(() => setRefreshTick((t) => t + 1)),
    ];
    return () => {
      for (const p of unlisteners) p.then((un) => un());
    };
  }, []);

  const recording = status?.recording ?? false;

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
      <span
        class="pill pill-recording pill-click"
        title="Open live captions"
        onClick={() =>
          status?.meeting_id != null &&
          setView({ kind: "transcript", meetingId: status.meeting_id })
        }
      >
        <span class="rec-dot" /> Recording {fmtElapsed(level?.elapsed_ms ?? 0)}
        <span class="level-meter" title="Mic / system levels">
          <span
            class="level-bar level-mic"
            style={{ width: `${Math.min(100, (level?.mic_rms ?? 0) * 300)}%` }}
          />
          <span
            class="level-bar level-loop"
            style={{ width: `${Math.min(100, (level?.loopback_rms ?? 0) * 300)}%` }}
          />
        </span>
      </span>
    );
  } else if (progress) {
    statusPill = (
      <span class="pill pill-busy">
        Transcribing ({progress.stage}) {Math.round(progress.pct)}%
      </span>
    );
  } else if (status?.transcribing_meeting_id != null) {
    // App opened mid-job: no progress event received yet.
    statusPill = <span class="pill pill-busy">Transcribing…</span>;
  } else {
    statusPill = <span class="pill pill-idle">Idle</span>;
  }

  return (
    <div class="app">
      <header class="topbar">
        <button
          class={recording ? "btn btn-with-icon btn-stop" : "btn btn-with-icon btn-record"}
          title={recording ? "Stop recording" : "Start recording"}
          aria-label={recording ? "Stop recording" : "Start recording"}
          onClick={() => (recording ? stopRecording() : startRecording()).catch((e) => alert(String(e)))}
        >
          {recording ? <Stop size={17} /> : <Record size={17} />}
        </button>
        {statusPill}
        <nav class="nav">
          <button
            class={view.kind === "meetings" ? "nav-btn active" : "nav-btn"}
            onClick={() => setView({ kind: "meetings" })}
          >
            Meetings
          </button>
          <button
            class={view.kind === "people" ? "nav-btn active" : "nav-btn"}
            onClick={() => setView({ kind: "people" })}
          >
            People
          </button>
          <button
            class={view.kind === "settings" ? "nav-btn nav-btn-icon active" : "nav-btn nav-btn-icon"}
            title="Settings"
            aria-label="Settings"
            onClick={() => setView({ kind: "settings" })}
          >
            <GearSix />
          </button>
        </nav>
      </header>
      <main class="content">
        {(view.kind === "meetings" || view.kind === "search") && (
          <div class="list-toolbar">
            <input
              class="search-input"
              type="search"
              placeholder="Search transcripts…"
              value={searchText}
              onInput={(e) => onSearchInput((e.target as HTMLInputElement).value)}
            />
          </div>
        )}
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
              setView({ kind: "transcript", meetingId, segmentId })
            }
          />
        )}
        {view.kind === "settings" && <SettingsView watcher={status?.watcher ?? null} />}
      </main>
    </div>
  );
}
