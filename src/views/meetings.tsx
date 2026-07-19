import { useEffect, useState } from "preact/hooks";
import {
  deleteMeeting,
  emptyRecycleBin,
  listDeletedMeetings,
  listMeetings,
  purgeMeeting,
  renameMeeting,
  restoreMeeting,
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

const PAGE = 50;

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
    <span class={`status-icon status-${status}`} title={STATUS_LABEL[status] ?? status}>
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

  const load = (offset: number, append: boolean) =>
    listMeetings(offset, PAGE + 1).then((rows) => {
      setHasMore(rows.length > PAGE);
      const page = rows.slice(0, PAGE);
      setMeetings((prev) => (append ? [...prev, ...page] : page));
    });

  const loadBin = () => listDeletedMeetings().then(setBinned).catch(() => {});

  useEffect(() => {
    load(0, false);
    loadBin();
  }, [props.refreshTick]);

  const commitRename = (m: Meeting) => {
    const title = editText.trim();
    setEditingId(null);
    if (title && title !== m.title) {
      renameMeeting(m.id, title).then(() =>
        setMeetings((prev) =>
          prev.map((x) => (x.id === m.id ? { ...x, title } : x)),
        ),
      );
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
      loadBin();
    });
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
    purgeMeeting(m.id).then(loadBin);
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
    emptyRecycleBin().then(loadBin);
  };

  const bin = binned.length > 0 && (
    <div class="recycle-bin">
      <div class="recycle-head" onClick={() => setBinOpen(!binOpen)}>
        <Trash size={14} />
        <span>Recycle bin ({binned.length})</span>
        <span class="muted">{binOpen ? "▾" : "▸"}</span>
        {binOpen && (
          <button
            class="btn btn-ghost recycle-empty"
            onClick={(e) => {
              e.stopPropagation();
              onEmptyBin(e.shiftKey);
            }}
          >
            Empty bin
          </button>
        )}
      </div>
      {binOpen &&
        binned.map((m) => (
          <div class="meeting-row recycle-row" key={m.id}>
            <div class="meeting-main">
              <span class="meeting-title">{m.title}</span>
              <span class="meeting-meta">
                deleted {fmtDate(m.deleted_at)} · auto-removed after 30 days
              </span>
            </div>
            <button
              class="icon-btn"
              title="Restore"
              onClick={() => restoreMeeting(m.id).then(() => { load(0, false); loadBin(); })}
            >
              <ArrowCounterClockwise size={16} />
            </button>
            <button
              class="icon-btn"
              title="Delete forever"
              onClick={(e) => onPurge(m, e.shiftKey)}
            >
              <Trash size={16} />
            </button>
          </div>
        ))}
    </div>
  );

  if (meetings.length === 0) {
    return (
      <div class="meeting-list">
        <div class="empty">
          <p>No meetings yet.</p>
          <p class="muted">Join a meeting or hit record.</p>
        </div>
        {bin}
      </div>
    );
  }

  return (
    <div class="meeting-list">
      {meetings.map((m) => (
        <div class="meeting-row" key={m.id}>
          <div class="meeting-main" onClick={() => props.onOpen(m.id)}>
            {editingId === m.id ? (
              <input
                class="rename-input"
                value={editText}
                autoFocus
                onClick={(e) => e.stopPropagation()}
                onInput={(e) => setEditText((e.target as HTMLInputElement).value)}
                onBlur={() => commitRename(m)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitRename(m);
                  if (e.key === "Escape") setEditingId(null);
                }}
              />
            ) : (
              <span
                class="meeting-title"
                title="Double-click to rename"
                onDblClick={(e) => {
                  e.stopPropagation();
                  setEditingId(m.id);
                  setEditText(m.title);
                }}
              >
                {m.title}
              </span>
            )}
            <span class="meeting-meta">
              {fmtDate(m.started_at)} · {fmtDuration(m.duration_ms)}
              {m.trigger === "auto" ? " · auto" : ""}
            </span>
          </div>
          <StatusIcon status={m.status} />
          <button
            class="icon-btn"
            title={editingId === m.id ? "Done" : "Rename"}
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
            class="icon-btn"
            title="Delete"
            onClick={(e) => onDelete(m, e.shiftKey)}
          >
            <Trash size={16} />
          </button>
        </div>
      ))}
      {hasMore && (
        <button class="btn btn-ghost" onClick={() => load(meetings.length, true)}>
          Load more
        </button>
      )}
      {bin}
    </div>
  );
}
