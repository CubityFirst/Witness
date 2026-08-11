import { useEffect, useRef, useState } from "preact/hooks";
import {
  deleteMeeting,
  emptyRecycleBin,
  listDeletedMeetings,
  listMeetings,
  purgeMeeting,
  renameMeeting,
  restoreMeeting,
  retranscribe,
  type DeletedMeeting,
  type Meeting,
} from "../lib/api";
import {
  ArrowCounterClockwise,
  CheckCircle,
  CircleNotch,
  PencilSimple,
  Record,
  Trash,
  WarningCircle,
  Waveform,
} from "../lib/icons";
import { appConfirm } from "../lib/confirm";
import { notifyError, notifySuccess } from "../lib/notify";

const PAGE = 50;

// Survives unmount so Back returns to the same number of loaded pages.
let loadedCount = 0;

export function fmtDate(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleString(undefined, {
    weekday: "short",
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function fmtDuration(ms: number | null): string {
  if (ms == null) return "—";
  const s = Math.round(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 0) return `${h} h ${m} min`;
  if (m > 0) return `${m} min`;
  return `${s} s`;
}

const STATUS_LABEL: Record<string, string> = {
  recording: "Recording",
  recorded: "Recorded — not transcribed yet",
  processing: "Transcribing…",
  transcribed: "Transcribed",
  failed: "Transcription failed",
};

/** Compact meeting-status indicator (tooltip carries the words). */
export function StatusIcon({ status }: { status: string }) {
  const icon =
    status === "recording" ? (
      <Record size={16} />
    ) : status === "recorded" ? (
      <Waveform size={16} />
    ) : status === "processing" ? (
      <CircleNotch size={16} />
    ) : status === "failed" ? (
      <WarningCircle size={16} />
    ) : (
      <CheckCircle size={16} />
    );
  return (
    <span
      class={`status-icon status-${status}`}
      title={STATUS_LABEL[status] ?? status}
      role="img"
      aria-label={STATUS_LABEL[status] ?? status}
    >
      {icon}
    </span>
  );
}

export function MeetingsView(props: {
  refreshTick: number;
  onOpen: (meetingId: number) => void;
}) {
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editText, setEditText] = useState("");
  const [binOpen, setBinOpen] = useState(false);
  const [binned, setBinned] = useState<DeletedMeeting[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const listRequest = useRef(0);
  const binRequest = useRef(0);
  const openTimer = useRef<number | null>(null);

  const load = async (offset: number, append: boolean, limit = PAGE) => {
    const request = ++listRequest.current;
    setLoading(true);
    try {
      const rows = await listMeetings(offset, limit + 1);
      if (request !== listRequest.current) return;
      setHasMore(rows.length > limit);
      const page = rows.slice(0, limit);
      setMeetings((prev) => (append ? [...prev, ...page] : page));
      setLoadError(null);
    } catch (error) {
      if (request !== listRequest.current) return;
      setLoadError(String(error));
      notifyError("Could not load meetings", error);
    } finally {
      if (request === listRequest.current) setLoading(false);
    }
  };

  const loadBin = async () => {
    const request = ++binRequest.current;
    try {
      const rows = await listDeletedMeetings();
      if (request === binRequest.current) setBinned(rows);
    } catch (error) {
      if (request === binRequest.current)
        notifyError("Could not load the recycle bin", error);
    }
  };

  useEffect(() => {
    // Re-request everything already loaded so background refreshes and
    // back-navigation don't reset pagination to page 1.
    void load(0, false, Math.max(PAGE, loadedCount));
    void loadBin();
    return () => {
      listRequest.current += 1;
      binRequest.current += 1;
    };
  }, [props.refreshTick]);

  useEffect(() => {
    if (meetings.length > 0) loadedCount = meetings.length;
  }, [meetings]);

  useEffect(
    () => () => {
      if (openTimer.current != null) window.clearTimeout(openTimer.current);
    },
    [],
  );

  const commitRename = (m: Meeting) => {
    const title = editText.trim();
    setEditingId(null);
    if (title && title !== m.title) {
      renameMeeting(m.id, title).then(() =>
        setMeetings((prev) =>
          prev.map((x) => (x.id === m.id ? { ...x, title } : x)),
        ),
      ).catch((error) => notifyError("Could not rename the meeting", error));
    }
  };

  // Shift-click skips the confirmation on all deletion actions.
  const onDelete = async (m: Meeting, skipConfirm: boolean) => {
    if (
      !skipConfirm &&
      !(await appConfirm(
        `Move "${m.title}" to the recycle bin?\nItems there are permanently deleted after 30 days.`,
        "Move to bin",
      ))
    )
      return;
    deleteMeeting(m.id).then(() => {
      setMeetings((prev) => prev.filter((x) => x.id !== m.id));
      void loadBin();
      notifySuccess("Meeting moved to the recycle bin", {
        label: "Undo",
        onClick: () => {
          restoreMeeting(m.id)
            .then(() => {
              void load(0, false, Math.max(PAGE, loadedCount));
              void loadBin();
              notifySuccess("Meeting restored");
            })
            .catch((error) => notifyError("Could not restore the meeting", error));
        },
      });
    }).catch((error) => notifyError("Could not move the meeting to the recycle bin", error));
  };

  const onPurge = async (m: DeletedMeeting, skipConfirm: boolean) => {
    if (
      !skipConfirm &&
      !(await appConfirm(
        `Permanently delete "${m.title}" (audio + transcript)?\nThis cannot be undone.`,
        "Delete forever",
      ))
    )
      return;
    purgeMeeting(m.id)
      .then(() => {
        void loadBin();
        notifySuccess("Meeting permanently deleted");
      })
      .catch((error) => notifyError("Could not permanently delete the meeting", error));
  };

  const onEmptyBin = async (skipConfirm: boolean) => {
    if (
      !skipConfirm &&
      !(await appConfirm(
        `Permanently delete all ${binned.length} meeting(s) in the recycle bin?\nThis cannot be undone.`,
        "Empty bin",
      ))
    )
      return;
    emptyRecycleBin()
      .then(() => {
        void loadBin();
        notifySuccess("Recycle bin emptied");
      })
      .catch((error) => notifyError("Could not empty the recycle bin", error));
  };

  const bin = binned.length > 0 && (
    <div class="recycle-bin">
      <div class="recycle-head-row">
        <button
          type="button"
          class="recycle-head"
          aria-expanded={binOpen}
          aria-controls="recycle-bin-items"
          onClick={() => setBinOpen(!binOpen)}
        >
          <Trash size={14} />
          <span>Recycle bin ({binned.length})</span>
          <span class="muted" aria-hidden="true">{binOpen ? "▾" : "▸"}</span>
        </button>
        {binOpen && (
          <button
            type="button"
            class="btn btn-ghost recycle-empty"
            onClick={(e) => onEmptyBin(e.shiftKey)}
          >
            Empty bin
          </button>
        )}
      </div>
      <div id="recycle-bin-items">
      {binOpen &&
        binned.map((m) => (
          <div class="meeting-row recycle-row" key={m.id}>
            <div class="meeting-main">
              <span class="meeting-title">{m.title}</span>
              <span class="meeting-meta">
                {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)} · deleted{" "}
                {fmtDate(m.deleted_at)} · auto-removed after 30 days
              </span>
            </div>
            <button
              type="button"
              class="icon-btn"
              title="Restore"
              aria-label={`Restore ${m.title}`}
              onClick={() =>
                restoreMeeting(m.id)
                  .then(() => {
                    void load(0, false, Math.max(PAGE, loadedCount));
                    void loadBin();
                    notifySuccess("Meeting restored");
                  })
                  .catch((error) => notifyError("Could not restore the meeting", error))
              }
            >
              <ArrowCounterClockwise size={16} />
            </button>
            <button
              type="button"
              class="icon-btn icon-btn-danger"
              title="Delete forever"
              aria-label={`Permanently delete ${m.title}`}
              onClick={(e) => onPurge(m, e.shiftKey)}
            >
              <Trash size={16} />
            </button>
          </div>
        ))}
      </div>
    </div>
  );

  if (loading && meetings.length === 0) {
    return <div class="empty" role="status">Loading meetings…</div>;
  }

  if (loadError && meetings.length === 0) {
    return (
      <div class="view-error" role="alert">
        <p>Could not load meetings: {loadError}</p>
        <button type="button" class="btn" onClick={() => void load(0, false)}>
          Try again
        </button>
      </div>
    );
  }

  if (meetings.length === 0) {
    return (
      <div class="meeting-list">
        <h2 class="sr-only">Meetings</h2>
        <div class="empty">
          <p>No meetings yet.</p>
          <p class="muted">Join a meeting or hit record.</p>
        </div>
        {bin}
      </div>
    );
  }

  return (
    <div class="meeting-list" role="list">
      <h2 class="sr-only">Meetings</h2>
      {meetings.map((m) => (
        <div class="meeting-row" role="listitem" key={m.id}>
          {editingId === m.id ? (
            <div class="meeting-main">
              <input
                class="rename-input"
                value={editText}
                autoFocus
                aria-label={`Rename ${m.title}`}
                onInput={(e) => setEditText((e.target as HTMLInputElement).value)}
                onBlur={() => commitRename(m)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitRename(m);
                  if (e.key === "Escape") setEditingId(null);
                }}
              />
              <span class="meeting-meta">
                {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)}
                {m.trigger === "auto" ? " · auto" : ""}
              </span>
            </div>
          ) : (
            <button
              type="button"
              class="meeting-main meeting-open"
              onClick={(e) => {
                // Delay single-click opens so the first click of a
                // double-click (rename) doesn't navigate away.
                if (e.detail > 1) return;
                if (openTimer.current != null) window.clearTimeout(openTimer.current);
                if (e.detail === 0) {
                  props.onOpen(m.id); // keyboard activation
                  return;
                }
                openTimer.current = window.setTimeout(() => {
                  openTimer.current = null;
                  props.onOpen(m.id);
                }, 250);
              }}
              onDblClick={(e) => {
                e.preventDefault();
                if (openTimer.current != null) {
                  window.clearTimeout(openTimer.current);
                  openTimer.current = null;
                }
                setEditingId(m.id);
                setEditText(m.title);
              }}
              aria-label={`Open ${m.title}, ${STATUS_LABEL[m.status] ?? m.status}`}
            >
              <span
                class="meeting-title"
                title="Double-click to rename"
              >
                {m.title}
              </span>
              <span class="meeting-meta">
                {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)}
                {m.trigger === "auto" ? " · auto" : ""}
              </span>
            </button>
          )}
          {m.status === "failed" || m.status === "recorded" ? (
            <button
              type="button"
              class={`icon-btn status-${m.status}`}
              title={
                m.status === "failed"
                  ? "Transcription failed — click to retry"
                  : "Not transcribed — click to transcribe"
              }
              aria-label={
                m.status === "failed"
                  ? `Retry transcription of ${m.title}`
                  : `Transcribe ${m.title}`
              }
              onClick={() =>
                retranscribe(m.id)
                  .then(() => notifySuccess("Transcription queued"))
                  .catch((error) => notifyError("Could not queue transcription", error))
              }
            >
              {m.status === "failed" ? <WarningCircle size={16} /> : <Waveform size={16} />}
            </button>
          ) : (
            <StatusIcon status={m.status} />
          )}
          <button
            type="button"
            class="icon-btn"
            title={editingId === m.id ? "Done" : "Rename"}
            aria-label={editingId === m.id ? `Save ${m.title}` : `Rename ${m.title}`}
            // Keep the input from blurring (and committing) before our
            // click handler decides what to do.
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              if (editingId === m.id) {
                commitRename(m); // saves if changed, closes the editor
              } else {
                setEditingId(m.id);
                setEditText(m.title);
              }
            }}
          >
            <PencilSimple size={16} />
          </button>
          <button
            type="button"
            class="icon-btn icon-btn-danger"
            title="Delete"
            aria-label={`Move ${m.title} to the recycle bin`}
            onClick={(e) => onDelete(m, e.shiftKey)}
          >
            <Trash size={16} />
          </button>
        </div>
      ))}
      {hasMore && (
        <button
          type="button"
          class="btn btn-ghost"
          disabled={loading}
          onClick={() => void load(meetings.length, true)}
        >
          {loading ? "Loading…" : "Load more"}
        </button>
      )}
      {bin}
    </div>
  );
}
