// Regenerates every Witness icon from the "Sheltered Sound" shield:
// crates/witness/icons/* (exe icon, window icon, tray icons), app-icon.png
// and assets/icon.svg.
//
//   node assets/brand/icons.mjs
//
// No npm dependencies: headless Chrome (or Edge) rasterises the SVGs; set
// WITNESS_CHROMIUM to pick the browser. Sizes up to 32 px are separate
// grid-fitted drawings, not downscales, so the three bars stay crisp in the
// tray.
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const ROOT = join(import.meta.dirname, "..", "..");
const ICONS = join(ROOT, "crates", "witness", "icons");
// Chrome first: Edge's launcher can hand headless runs to its startup-boost
// process, which then prints nothing.
const BROWSER =
  process.env.WITNESS_CHROMIUM ??
  [
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  ].find((path) => existsSync(path));

const TEAL = "#14363E";
const MINT = "#9AE6CC";
const CORAL = "#E5484D";

// Shield outline on a 100-unit grid.
const SHIELD =
  "M46.8 14.6Q50 12.9 53.2 14.6C61 19 68 21.6 77.6 23.3Q82.2 24.2 82.5 28.8C82.9 45 82.3 55 80.2 62C76.5 74.5 66 80.5 52.5 85.6Q50 86.6 47.5 85.6C34 80.5 23.5 74.5 19.8 62C17.7 55 17.1 45 17.5 28.8Q17.8 24.2 22.4 23.3C32 21.6 39 19 46.8 14.6Z";
const SHIELD_CENTER = { x: 50, y: 49.75 };

// Bars are [x, width, top, bottom] in output pixels, so every edge lands on a
// whole pixel. The recording dot [r, halo] is centred on the tile's corner
// arc so it never leaves the tile, and each halo clears the right bar's end.
const FITS = {
  16: { rx: 3, scale: 0.18, bars: [[4, 2, 6, 10], [7, 2, 4, 12], [10, 2, 6, 10]], dot: [2.5, 3.25] },
  20: { rx: 3.5, scale: 0.225, bars: [[5, 2, 7, 13], [9, 2, 5, 15], [13, 2, 7, 13]], dot: [3, 3.9] },
  24: { rx: 4.5, scale: 0.27, bars: [[7, 2, 9, 15], [11, 2, 6, 18], [15, 2, 9, 15]], dot: [3.8, 4.8] },
  32: { rx: 6, scale: 0.36, bars: [[8, 4, 12, 20], [14, 4, 8, 24], [20, 4, 12, 20]], dot: [5, 6.3] },
};
// The 100-unit vector master, used from 48 px up.
const MASTER = {
  rx: 18,
  scale: 1,
  bars: [[31.1, 9.2, 38.8, 61], [45.4, 9.2, 28.7, 69.4], [59.6, 9.2, 38.8, 61]],
  dot: [15, 20],
};

function svg(size, recording) {
  const fit = FITS[size] ?? MASTER;
  const grid = FITS[size] ? size : 100;
  const s = fit.scale;
  const tx = grid / 2 - SHIELD_CENTER.x * s;
  const ty = grid / 2 - SHIELD_CENTER.y * s;
  const bars = fit.bars
    .map(([x, w, top, bottom]) => `<rect x="${x}" y="${top}" width="${w}" height="${+(bottom - top).toFixed(3)}" rx="${w / 2}"/>`)
    .join("");
  const [r, halo] = fit.dot;
  const c = grid - fit.rx;
  const dot = recording
    ? `<circle cx="${c}" cy="${c}" r="${halo}" fill="${TEAL}"/><circle cx="${c}" cy="${c}" r="${r}" fill="${CORAL}"/>`
    : "";
  const dims = size === null ? "" : ` width="${size}" height="${size}"`;
  return (
    `<svg xmlns="http://www.w3.org/2000/svg"${dims} viewBox="0 0 ${grid} ${grid}">` +
    `<clipPath id="t"><rect width="${grid}" height="${grid}" rx="${fit.rx}"/></clipPath><g clip-path="url(#t)">` +
    `<rect width="${grid}" height="${grid}" fill="${TEAL}"/>` +
    `<path fill="${MINT}" transform="translate(${+tx.toFixed(3)} ${+ty.toFixed(3)}) scale(${s})" d="${SHIELD}"/>` +
    `<g fill="${TEAL}">${bars}</g>${dot}</g></svg>`
  );
}

function render(items) {
  const dir = mkdtempSync(join(tmpdir(), "witness-icons-"));
  try {
    const page = join(dir, "render.html");
    writeFileSync(
      page,
      `<!doctype html><pre id="out"></pre><script>
(async () => {
  const out = {};
  for (const it of ${JSON.stringify(items)}) {
    const img = new Image();
    img.src = "data:image/svg+xml;base64," + btoa(it.svg);
    await img.decode();
    const c = document.createElement("canvas");
    c.width = c.height = it.size;
    c.getContext("2d").drawImage(img, 0, 0, it.size, it.size);
    out[it.name] = c.toDataURL("image/png");
  }
  document.getElementById("out").textContent = "@@BEG" + "IN@@" + JSON.stringify(out) + "@@E" + "ND@@";
})();
</script>`,
    );
    if (!BROWSER) throw new Error("Chrome or Edge not found; set WITNESS_CHROMIUM");
    const dump = execFileSync(
      BROWSER,
      // Without --enable-logging, Chromium on Windows may not attach to our
      // stdout and --dump-dom prints nothing.
      ["--headless=new", "--disable-gpu", "--enable-logging=stderr", "--log-level=3", `--user-data-dir=${join(dir, "profile")}`, "--virtual-time-budget=10000", "--dump-dom", pathToFileURL(page).href],
      { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] },
    );
    const start = dump.indexOf("@@BEGIN@@");
    const end = dump.indexOf("@@END@@", start);
    if (start === -1 || end === -1) throw new Error(`${BROWSER} returned no rendered icons; try WITNESS_CHROMIUM`);
    const json = dump.slice(start + "@@BEGIN@@".length, end);
    return Object.fromEntries(
      Object.entries(JSON.parse(json)).map(([name, url]) => [name, Buffer.from(url.split(",")[1], "base64")]),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// PNG-compressed frames; 20 px is the small-icon size at 125 % scaling.
function ico(frames) {
  const header = Buffer.alloc(6 + 16 * frames.length);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(frames.length, 4);
  let offset = header.length;
  frames.forEach(({ size, data }, i) => {
    const entry = 6 + 16 * i;
    header.writeUInt8(size % 256, entry);
    header.writeUInt8(size % 256, entry + 1);
    header.writeUInt16LE(1, entry + 4);
    header.writeUInt16LE(32, entry + 6);
    header.writeUInt32LE(data.length, entry + 8);
    header.writeUInt32LE(offset, entry + 12);
    offset += data.length;
  });
  return Buffer.concat([header, ...frames.map((frame) => frame.data)]);
}

const png = render([
  ...[16, 20, 24, 32, 48, 64, 128, 256, 512, 1024].map((size) => ({ name: `idle-${size}`, size, svg: svg(size, false) })),
  { name: "rec-128", size: 128, svg: svg(128, true) },
]);

writeFileSync(join(ICONS, "icon.ico"), ico([32, 16, 20, 24, 48, 64, 256].map((size) => ({ size, data: png[`idle-${size}`] }))));
writeFileSync(join(ICONS, "32x32.png"), png["idle-32"]);
writeFileSync(join(ICONS, "128x128.png"), png["idle-128"]);
writeFileSync(join(ICONS, "128x128@2x.png"), png["idle-256"]);
writeFileSync(join(ICONS, "icon.png"), png["idle-512"]);
writeFileSync(join(ICONS, "tray-rec.png"), png["rec-128"]);
writeFileSync(join(ROOT, "app-icon.png"), png["idle-1024"]);
writeFileSync(join(ROOT, "assets", "icon.svg"), svg(null, false) + "\n");
console.log("Wrote crates/witness/icons/*, app-icon.png and assets/icon.svg");
