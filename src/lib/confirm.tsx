import { useEffect, useState } from "preact/hooks";

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
    // A newer request cancels any dangling one.
    currentResolver?.(false);
    currentResolver = resolve;
    showFn?.({ message, okLabel });
  });
}

export function ConfirmHost() {
  const [req, setReq] = useState<Request | null>(null);

  useEffect(() => {
    showFn = setReq;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && currentResolver) {
        currentResolver(false);
        currentResolver = null;
        setReq(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      showFn = null;
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  if (!req) return null;

  const answer = (v: boolean) => {
    currentResolver?.(v);
    currentResolver = null;
    setReq(null);
  };

  return (
    <div class="modal-backdrop" onClick={() => answer(false)}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <p class="modal-message">{req.message}</p>
        <div class="modal-actions">
          <button class="btn btn-ghost" onClick={() => answer(false)}>
            Cancel
          </button>
          <button class="btn btn-danger" autoFocus onClick={() => answer(true)}>
            {req.okLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
