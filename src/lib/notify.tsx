import { useEffect, useRef, useState } from "preact/hooks";

type NoticeKind = "error" | "info" | "success";

interface NoticeAction {
  label: string;
  onClick: () => void;
}

interface Notice {
  id: number;
  kind: NoticeKind;
  message: string;
  expiresAt: number;
  action?: NoticeAction;
}

let nextId = 1;
let notices: Notice[] = [];
const listeners = new Set<(next: Notice[]) => void>();

function publish() {
  const snapshot = [...notices];
  for (const listener of listeners) listener(snapshot);
}

function addNotice(kind: NoticeKind, message: string, action?: NoticeAction) {
  const clean = message.trim();
  if (!clean) return;

  const existing = notices.find((notice) => notice.kind === kind && notice.message === clean);
  if (existing) {
    existing.expiresAt = action
      ? Number.POSITIVE_INFINITY
      : Date.now() + (kind === "error" ? 9000 : 5000);
    existing.action = action;
  } else {
    notices = [
      ...notices.slice(-3),
      {
        id: nextId++,
        kind,
        message: clean,
        // Notices that require a choice remain until the user acts or
        // dismisses them; keyboard users must not race a short timeout.
        expiresAt: action
          ? Number.POSITIVE_INFINITY
          : Date.now() + (kind === "error" ? 9000 : 5000),
        action,
      },
    ];
  }
  publish();
}

function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

export function notifyError(message: string, error?: unknown, action?: NoticeAction) {
  addNotice(
    "error",
    error === undefined ? message : `${message}: ${describeError(error)}`,
    action,
  );
}

export function notifyInfo(message: string, action?: NoticeAction) {
  addNotice("info", message, action);
}

export function notifySuccess(message: string, action?: NoticeAction) {
  addNotice("success", message, action);
}

function dismiss(id: number) {
  notices = notices.filter((notice) => notice.id !== id);
  publish();
}

/** A single, app-level host for accessible non-blocking notifications. */
export function NotificationHost() {
  const [visible, setVisible] = useState<Notice[]>(notices);
  // Expiry pauses while the pointer is over the toast region.
  const [paused, setPaused] = useState(false);
  const pausedAt = useRef(0);

  useEffect(() => {
    listeners.add(setVisible);
    setVisible([...notices]);
    return () => listeners.delete(setVisible);
  }, []);

  useEffect(() => {
    if (visible.length === 0 || paused) return;
    const expirations = visible
      .map((notice) => notice.expiresAt)
      .filter(Number.isFinite);
    if (expirations.length === 0) return;
    const nextExpiry = Math.min(...expirations);
    const timer = window.setTimeout(() => {
      const now = Date.now();
      notices = notices.filter((notice) => notice.expiresAt > now);
      publish();
    }, Math.max(0, nextExpiry - Date.now()));
    return () => window.clearTimeout(timer);
  }, [visible, paused]);

  if (visible.length === 0) return null;

  return (
    <div
      class="toast-region"
      aria-label="Notifications"
      aria-live="polite"
      onMouseEnter={() => {
        pausedAt.current = Date.now();
        setPaused(true);
      }}
      onMouseLeave={() => {
        if (pausedAt.current === 0) return;
        const delta = Date.now() - pausedAt.current;
        pausedAt.current = 0;
        notices = notices.map((notice) => ({
          ...notice,
          expiresAt: notice.expiresAt + delta,
        }));
        publish();
        setPaused(false);
      }}
    >
      {visible.map((notice) => (
        <div
          class={`toast toast-${notice.kind}`}
          key={notice.id}
          role={notice.kind === "error" ? "alert" : "status"}
        >
          <span>{notice.message}</span>
          {notice.action && (
            <button
              type="button"
              class="btn btn-ghost toast-action"
              onClick={() => {
                notice.action?.onClick();
                dismiss(notice.id);
              }}
            >
              {notice.action.label}
            </button>
          )}
          <button
            type="button"
            class="toast-dismiss"
            aria-label="Dismiss notification"
            onClick={() => dismiss(notice.id)}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
