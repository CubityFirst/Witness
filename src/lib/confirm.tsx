import { useEffect, useRef, useState } from "preact/hooks";

/**
 * In-app confirmation dialog. Views call `appConfirm(...)` (promise-based);
 * the <ConfirmHost/> mounted once in App renders the modal.
 */

interface Request {
  message: string;
  okLabel: string;
}

let currentResolver: ((v: boolean) => void) | null = null;
let showFn: ((req: Request) => void) | null = null;

export function appConfirm(message: string, okLabel = "Delete"): Promise<boolean> {
  return new Promise((resolve) => {
    if (!showFn) {
      resolve(false);
      return;
    }
    // A newer request cancels any dangling one.
    currentResolver?.(false);
    currentResolver = resolve;
    showFn({ message, okLabel });
  });
}

export function ConfirmHost() {
  const [req, setReq] = useState<Request | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  const answer = (value: boolean) => {
    currentResolver?.(value);
    currentResolver = null;
    setReq(null);
    requestAnimationFrame(() => previousFocus.current?.focus());
  };

  useEffect(() => {
    showFn = (request) => {
      previousFocus.current = document.activeElement as HTMLElement | null;
      setReq(request);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && currentResolver) {
        e.preventDefault();
        e.stopPropagation();
        answer(false);
      } else if (e.key === "Tab" && currentResolver) {
        const focusable = dialogRef.current?.querySelectorAll<HTMLElement>(
          'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
        );
        if (!focusable?.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      showFn = null;
      currentResolver?.(false);
      currentResolver = null;
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  if (!req) return null;

  return (
    <div
      class="modal-backdrop"
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) answer(false);
      }}
    >
      <div
        class="modal"
        ref={dialogRef}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-message"
      >
        <p class="modal-message" id="confirm-message">
          {req.message}
        </p>
        <div class="modal-actions">
          <button
            type="button"
            class="btn btn-ghost"
            autoFocus
            onClick={() => answer(false)}
          >
            Cancel
          </button>
          <button type="button" class="btn btn-danger" onClick={() => answer(true)}>
            {req.okLabel}
          </button>
        </div>
        <p class="muted modal-tip">Tip: hold Shift to skip this confirmation</p>
      </div>
    </div>
  );
}
