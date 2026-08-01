import { useEffect, useState } from "preact/hooks";

type NoticeKind = "error" | "info" | "success";

interface Notice {
  id: number;
  kind: NoticeKind;
  message: string;
  expiresAt: number;
}

let nextId = 1;
let notices: Notice[] = [];
const listeners = new Set<(next: Notice[]) => void>();

function publish() {
  const snapshot = [...notices];
  for (const listener of listeners) listener(snapshot);
}

function addNotice(kind: NoticeKind, message: string) {
  const clean = message.trim();
  if (!clean) return;

  const existing = notices.find((notice) => notice.kind === kind && notice.message === clean);
  if (existing) {
    existing.expiresAt = Date.now() + (kind === "error" ? 9000 : 5000);
  } else {
    notices = [
      ...notices.slice(-3),
      {
        id: nextId++,
        kind,
        message: clean,
        expiresAt: Date.now() + (kind === "error" ? 9000 : 5000),
      },
    ];
  }
  publish();
}

function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

export function notifyError(message: string, error?: unknown) {
  addNotice(
    "error",
    error === undefined ? message : `${message}: ${describeError(error)}`,
  );
}

export function notifyInfo(message: string) {
  addNotice("info", message);
}

export function notifySuccess(message: string) {
  addNotice("success", message);
}

function dismiss(id: number) {
  notices = notices.filter((notice) => notice.id !== id);
  publish();
}

/** A single, app-level host for accessible non-blocking notifications. */
export function NotificationHost() {
  const [visible, setVisible] = useState<Notice[]>(notices);

  useEffect(() => {
    listeners.add(setVisible);
    setVisible([...notices]);
    return () => listeners.delete(setVisible);
  }, []);

  useEffect(() => {
    if (visible.length === 0) return;
    const nextExpiry = Math.min(...visible.map((notice) => notice.expiresAt));
    const timer = window.setTimeout(() => {
      const now = Date.now();
      notices = notices.filter((notice) => notice.expiresAt > now);
      publish();
    }, Math.max(0, nextExpiry - Date.now()));
    return () => window.clearTimeout(timer);
  }, [visible]);

  if (visible.length === 0) return null;

  return (
    <div class="toast-region" aria-label="Notifications" aria-live="polite">
      {visible.map((notice) => (
        <div
          class={`toast toast-${notice.kind}`}
          key={notice.id}
          role={notice.kind === "error" ? "alert" : "status"}
        >
          <span>{notice.message}</span>
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
