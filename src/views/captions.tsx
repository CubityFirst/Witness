import { useEffect, useState } from "preact/hooks";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { onLiveTranscript, type LiveTranscript } from "../lib/events";

/**
 * Content of the small always-on-top captions window ("captions" webview,
 * routed via #captions). Shows the last few live lines; the window itself
 * is opened/closed by the backend with the recording.
 */
export function CaptionsOverlay() {
  const [lines, setLines] = useState<LiveTranscript[]>([]);

  useEffect(() => {
    const un = onLiveTranscript((line) =>
      setLines((prev) => [...prev.slice(-2), line]),
    );
    return () => {
      un.then((u) => u());
    };
  }, []);

  return (
    // data-tauri-drag-region only applies to the exact element, not its
    // children — start the drag ourselves so the whole window is grabbable.
    <div
      class="captions-overlay"
      onMouseDown={(e) => {
        if (e.buttons === 1) {
          getCurrentWindow().startDragging().catch(() => {});
        }
      }}
    >
      <div class="captions-grip">● Witness — live captions (drag to move)</div>
      <div class="captions-lines">
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
