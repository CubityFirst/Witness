import { render } from "preact";
import { App } from "./app";
import { CaptionsOverlay } from "./views/captions";
import "./styles.css";

// Suppress the webview's browser context menu (back/reload/inspect…) —
// keep it only inside text fields, where cut/copy/paste is genuinely useful.
document.addEventListener("contextmenu", (e) => {
  const t = e.target as HTMLElement | null;
  const inField =
    t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
  if (!inField) e.preventDefault();
});

// The floating captions window loads the same bundle with #captions.
const root = document.getElementById("root")!;
if (window.location.hash === "#captions") {
  document.body.classList.add("captions-body");
  render(<CaptionsOverlay />, root);
} else {
  render(<App />, root);
}
