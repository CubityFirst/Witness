import { useEffect, useState } from "preact/hooks";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  onLiveCaptionsStatus,
  onLiveTranscript,
  type LiveTranscript,
} from "../lib/events";

/**
 * Content of the small always-on-top captions window ("captions" webview,
 * routed via #captions). Shows the last few live lines; the window itself
 * is opened/closed by the backend with the recording.
 */
export function CaptionsOverlay() {
  const [lines, setLines] = useState<LiveTranscript[]>([]);
  const [error, setError] = useState<string | null>(null);

  const closeOverlay = () =>
    getCurrentWindow()
      .close()
      .catch((closeError) => setError(`Could not close captions: ${String(closeError)}`));

  useEffect(() => {
    // ~20 lines of history: the flex-end + overflow:hidden layout means window
    // height controls how many are visible, so resizing taller shows more.
    const listeners = [
      onLiveTranscript((line) =>
        setLines((prev) => [...prev.slice(-19), line]),
      ),
      onLiveCaptionsStatus((status) => {
        if (!status.active) {
          setError(status.error ?? "Live captions stopped");
        }
      }),
    ].map((listener) =>
      listener.catch((listenError) => {
        setError(`Live captions disconnected: ${String(listenError)}`);
        return () => {};
      }),
    );
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") void closeOverlay();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      for (const listener of listeners) {
        listener.then((unlisten) => unlisten()).catch((detachError) =>
          console.error("Could not detach a live-caption listener", detachError),
        );
      }
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);

  return (
    // data-tauri-drag-region only applies to the exact element, not its
    // children — start the drag ourselves so the whole window is grabbable.
    <div
      class="captions-overlay"
      role="region"
      aria-label="Live captions"
      onMouseDown={(e) => {
        if (e.buttons === 1 && !(e.target as HTMLElement).closest("button")) {
          getCurrentWindow()
            .startDragging()
            .catch((dragError) => setError(`Could not move captions: ${String(dragError)}`));
        }
      }}
    >
      <div class="captions-grip">
        <span>● Witness — live captions (drag to move)</span>
        <button
          type="button"
          class="captions-close"
          aria-label="Close live captions"
          title="Close captions (Escape)"
          onClick={closeOverlay}
        >
          ×
        </button>
      </div>
      {error && <div class="captions-error" role="alert">{error}</div>}
      <div class="captions-lines" role="log" aria-live="polite" aria-relevant="additions">
        {lines.length === 0 && (
          <div class="captions-line captions-idle">Listening…</div>
        )}
        {lines.map((l) => (
          <div class="captions-line" key={`${l.track}-${l.start_ms}`}>
            <span class={l.track === "mic" ? "captions-me" : "captions-them"}>
              {l.track === "mic" ? "Me" : "Them"}
            </span>{" "}
            {l.text}
          </div>
        ))}
      </div>
    </div>
  );
}
