// Regenerates the README banners (assets/brand/witness-banner-{light,dark}.png):
// the app tile from assets/icon.svg beside the name and the tagline, centred
// on a rounded card. Run icons.mjs first if the tile changed.
//
//   node assets/brand/banner.mjs [out-dir]
//
// No npm dependencies: headless Chrome (or Edge) draws the card on a canvas
// with the system's Segoe UI Variable; set WITNESS_CHROMIUM to pick the
// browser. The PNGs are 2x for the README's 640 px width.
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const ROOT = join(import.meta.dirname, "..", "..");
const OUT = process.argv[2] ?? import.meta.dirname;
const BROWSER =
  process.env.WITNESS_CHROMIUM ??
  [
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  ].find((path) => existsSync(path));

const TILE = readFileSync(join(ROOT, "assets", "icon.svg"), "utf8").trim();

// Card geometry (output px) and type.
const CARD = {
  width: 1280, height: 352, radius: 28, border: 2,
  tile: 276, gap: 36, titleSize: 152, subSize: 26, lineGap: 30,
  title: "Witness",
  subtitle: "Local meeting recorder and transcriber for Windows 11",
};
const THEMES = {
  light: { bg: "#F3F6F5", border: "#DCE6E3", title: "#14363E", sub: "#4E6569" },
  dark: { bg: "#14363E", border: "#21505A", title: "#F3F6F5", sub: "#9FC4BE" },
};

const page = `<!doctype html><pre id="out"></pre><script>
(async () => {
  const card = ${JSON.stringify(CARD)};
  const themes = ${JSON.stringify(THEMES)};
  const tile = new Image();
  tile.src = "data:image/svg+xml;base64," + btoa(${JSON.stringify(TILE)});
  await tile.decode();
  const titleFont = "600 " + card.titleSize + "px 'Segoe UI Variable Display', 'Segoe UI'";
  const subFont = card.subSize + "px 'Segoe UI Variable Text', 'Segoe UI'";
  await document.fonts.load(titleFont);
  await document.fonts.load(subFont);
  const out = {};
  for (const [name, t] of Object.entries(themes)) {
    const c = document.createElement("canvas");
    c.width = card.width;
    c.height = card.height;
    const g = c.getContext("2d");
    const b = card.border / 2;
    g.beginPath();
    g.roundRect(b, b, card.width - card.border, card.height - card.border, card.radius);
    g.fillStyle = t.bg;
    g.fill();
    g.lineWidth = card.border;
    g.strokeStyle = t.border;
    g.stroke();

    // Measure the ink, not the em box, so the group centres optically.
    g.font = titleFont;
    const tm = g.measureText(card.title);
    g.font = subFont;
    const sm = g.measureText(card.subtitle);
    const textW = Math.max(tm.actualBoundingBoxRight + tm.actualBoundingBoxLeft, sm.actualBoundingBoxRight + sm.actualBoundingBoxLeft);
    const titleH = tm.actualBoundingBoxAscent + tm.actualBoundingBoxDescent;
    const subH = sm.actualBoundingBoxAscent + sm.actualBoundingBoxDescent;
    const textH = titleH + card.lineGap + subH;
    const left = Math.round((card.width - card.tile - card.gap - textW) / 2);
    const mid = card.height / 2;

    g.drawImage(tile, left, mid - card.tile / 2, card.tile, card.tile);
    const textX = left + card.tile + card.gap;
    const top = mid - textH / 2;
    g.fillStyle = t.title;
    g.font = titleFont;
    g.fillText(card.title, textX + tm.actualBoundingBoxLeft, top + tm.actualBoundingBoxAscent);
    g.fillStyle = t.sub;
    g.font = subFont;
    g.fillText(card.subtitle, textX + sm.actualBoundingBoxLeft, top + titleH + card.lineGap + sm.actualBoundingBoxAscent);
    out[name] = c.toDataURL("image/png");
  }
  document.getElementById("out").textContent = "@@BEG" + "IN@@" + JSON.stringify(out) + "@@E" + "ND@@";
})();
</script>`;

const dir = mkdtempSync(join(tmpdir(), "witness-banner-"));
try {
  const file = join(dir, "render.html");
  writeFileSync(file, page);
  if (!BROWSER) throw new Error("Chrome or Edge not found; set WITNESS_CHROMIUM");
  const dump = execFileSync(
    BROWSER,
    ["--headless=new", "--disable-gpu", "--enable-logging=stderr", "--log-level=3", `--user-data-dir=${join(dir, "profile")}`, "--virtual-time-budget=10000", "--dump-dom", pathToFileURL(file).href],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] },
  );
  const start = dump.indexOf("@@BEGIN@@");
  const end = dump.indexOf("@@END@@", start);
  if (start === -1 || end === -1) throw new Error(`${BROWSER} returned no banners; try WITNESS_CHROMIUM`);
  for (const [name, url] of Object.entries(JSON.parse(dump.slice(start + "@@BEGIN@@".length, end)))) {
    writeFileSync(join(OUT, `witness-banner-${name}.png`), Buffer.from(url.split(",")[1], "base64"));
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}
console.log(`Wrote witness-banner-{light,dark}.png to ${OUT}`);
