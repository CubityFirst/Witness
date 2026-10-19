// Regenerates every Witness icon from the "Sheltered Sound" shield:
// crates/witness/icons/* (exe icon, window icon, tray glyphs), app-icon.png
// and assets/icon.svg.
//
//   node assets/brand/icons.mjs
//
// No npm dependencies: headless Chrome (or Edge) rasterises the SVGs; set
// WITNESS_CHROMIUM to pick the browser. Sizes up to 32 px are separate
// grid-fitted drawings, not downscales, so the three bars stay crisp in the
// tray. Set WITNESS_ICON_PREVIEW to a folder to also get every frame as a
// PNG for checking by eye.
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
// Tray glyph ink per taskbar theme, as Windows' own notification-area icons.
const INK = { light: "#1C1C1C", dark: "#FFFFFF" };

// Shield outline on a 100-unit grid.
const SHIELD =
  "M46.8 14.6Q50 12.9 53.2 14.6C61 19 68 21.6 77.6 23.3Q82.2 24.2 82.5 28.8C82.9 45 82.3 55 80.2 62C76.5 74.5 66 80.5 52.5 85.6Q50 86.6 47.5 85.6C34 80.5 23.5 74.5 19.8 62C17.7 55 17.1 45 17.5 28.8Q17.8 24.2 22.4 23.3C32 21.6 39 19 46.8 14.6Z";
const SHIELD_CENTER = { x: 50, y: 49.75 };
// The bars at shield scale 1, as [x, width, top, bottom].
const MASTER_BARS = [[31.1, 9.2, 38.8, 61], [45.4, 9.2, 28.7, 69.4], [59.6, 9.2, 38.8, 61]];

// Grid fits: bars in output pixels so every edge lands on a whole pixel.
// `tile` / `glyph` scale the shield for the app tile and the bare tray glyph.
// The state badges are chosen so their halo never opens into a bar:
// `squares` [side, gap, clearance] for the glyph, and for the tile `dot`
// [r, halo] (centred on the corner arc) and `tileSquares` [side, gap,
// clearance, inset] (kept inside the rounded corner).
const FITS = {
  16: {
    rx: 3, tile: 0.18, glyph: 0.19, bars: [[4, 2, 6, 10], [7, 2, 4, 12], [10, 2, 6, 10]],
    squares: [3, 1, 1], dot: [2.5, 3.25], tileSquares: [2, 1, 1, 1],
  },
  20: {
    rx: 3.5, tile: 0.225, glyph: 0.2375, bars: [[5, 2, 7, 13], [9, 2, 5, 15], [13, 2, 7, 13]],
    squares: [4, 1, 1], dot: [3, 3.9], tileSquares: [3, 1, 1, 1],
  },
  24: {
    rx: 4.5, tile: 0.27, glyph: 0.285, bars: [[7, 2, 9, 15], [11, 2, 6, 18], [15, 2, 9, 15]],
    squares: [4, 1, 1], dot: [3.8, 4.8], tileSquares: [3, 1, 1, 2],
  },
  32: {
    rx: 6, tile: 0.36, glyph: 0.38, bars: [[8, 4, 12, 20], [14, 4, 8, 24], [20, 4, 12, 20]],
    squares: [6, 2, 2], dot: [5, 6.3], tileSquares: [4, 2, 2, 2],
  },
};
// The 100-unit vector master, used for every other size.
const MASTER = {
  rx: 18, tile: 1, glyph: 1.19,
  squares: [18.75, 6.25, 6.25], dot: [15, 20], tileSquares: [12, 4, 6, 6],
};

const fmt = (n) => +n.toFixed(3);
const rect = (x, y, w, h, extra = "") => `<rect x="${fmt(x)}" y="${fmt(y)}" width="${fmt(w)}" height="${fmt(h)}"${extra}/>`;

// The shield path and its bar rects (in output units) at `scale`.
function shieldParts(size, scale) {
  const fit = FITS[size];
  const grid = fit ? size : 100;
  const tx = grid / 2 - SHIELD_CENTER.x * scale;
  const ty = grid / 2 - SHIELD_CENTER.y * scale;
  const bars = fit
    ? fit.bars
    : MASTER_BARS.map(([x, w, top, bottom]) => [tx + x * scale, w * scale, ty + top * scale, ty + bottom * scale]);
  return {
    grid,
    path: `<path transform="translate(${fmt(tx)} ${fmt(ty)}) scale(${scale})" d="${SHIELD}"/>`,
    bars: bars.map(([x, w, top, bottom]) => rect(x, top, w, bottom - top, ` rx="${fmt(w / 2)}"`)).join(""),
  };
}

const open = (size, grid) =>
  `<svg xmlns="http://www.w3.org/2000/svg"${size ? ` width="${size}" height="${size}"` : ""} viewBox="0 0 ${grid} ${grid}">`;

// The app icon: mint shield on a rounded teal tile. The colour tray icons add
// the state badge on a teal halo in the corner.
function tile(size, state = "idle") {
  const fit = FITS[size] ?? MASTER;
  const { grid, path, bars } = shieldParts(size, fit.tile);
  let badge = "";
  if (state === "recording") {
    const [r, halo] = fit.dot;
    const c = grid - fit.rx;
    badge =
      `<circle cx="${c}" cy="${c}" r="${halo}" fill="${TEAL}"/>` +
      `<circle cx="${c}" cy="${c}" r="${r}" fill="${CORAL}"/>`;
  } else if (state === "transcribing") {
    const [side, gap, clear, inset] = fit.tileSquares;
    const right = grid - inset;
    const left = right - 2 * side - gap;
    badge =
      rect(left - clear, right - side - clear, grid - left + clear, grid - right + side + clear, ` fill="${TEAL}"`) +
      `<g fill="${MINT}">${rect(left, right - side, side, side)}${rect(right - side, right - side, side, side)}</g>`;
  }
  return (
    open(size, grid) +
    `<clipPath id="t"><rect width="${grid}" height="${grid}" rx="${fit.rx}"/></clipPath><g clip-path="url(#t)">` +
    `<rect width="${grid}" height="${grid}" fill="${TEAL}"/><g fill="${MINT}">${path}</g>` +
    `<g fill="${TEAL}">${bars}</g>${badge}</g></svg>`
  );
}

// A tray glyph: the bare shield in one ink with the bars cut through, plus
// the state badge in the bottom-right corner, cut clear of the shield. State
// is told by shape (dot / two squares), not colour alone.
function glyph(size, state, theme) {
  const fit = FITS[size] ?? MASTER;
  const { grid, path, bars } = shieldParts(size, fit.glyph);
  const ink = INK[theme];
  const u = grid / 16;
  let cut = "";
  let badge = "";
  if (state === "recording") {
    const c = grid - 2.5 * u;
    cut = `<circle cx="${fmt(c)}" cy="${fmt(c)}" r="${fmt(3.5 * u)}"/>`;
    badge = `<circle cx="${fmt(c)}" cy="${fmt(c)}" r="${fmt(2.5 * u)}" fill="${CORAL}"/>`;
  } else if (state === "transcribing") {
    const [side, gap, clear] = fit.squares;
    cut = rect(grid - 2 * side - gap - clear, grid - side - clear, 2 * side + gap + clear, side + clear);
    badge =
      `<g fill="${ink}">${rect(grid - side, grid - side, side, side)}` +
      `${rect(grid - 2 * side - gap, grid - side, side, side)}</g>`;
  }
  return (
    open(size, grid) +
    `<mask id="m"><rect width="${grid}" height="${grid}" fill="#fff"/><g fill="#000">${bars}${cut}</g></mask>` +
    `<g fill="${ink}" mask="url(#m)">${path}</g>${badge}</svg>`
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

// PNG-compressed frames.
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

// 20 px is the small-icon size at 125 % scaling; the tray glyphs cover every
// scale step up to 300 % (48 px).
const TILE_SIZES = [16, 20, 24, 32, 48, 64, 128, 256, 512, 1024];
const GLYPH_SIZES = [16, 20, 24, 28, 32, 40, 48];
const STATES = ["idle", "recording", "transcribing"];
// Monochrome for a light or dark taskbar, or the full-colour tile.
const STYLES = ["light", "dark", "colour"];
const trayIcon = (size, state, style) => (style === "colour" ? tile(size, state) : glyph(size, state, style));

const png = render([
  ...TILE_SIZES.map((size) => ({ name: `tile-${size}`, size, svg: tile(size) })),
  ...STATES.flatMap((state) =>
    STYLES.flatMap((style) =>
      GLYPH_SIZES.map((size) => ({ name: `${state}-${style}-${size}`, size, svg: trayIcon(size, state, style) })),
    ),
  ),
]);

writeFileSync(join(ICONS, "icon.ico"), ico([32, 16, 20, 24, 48, 64, 256].map((size) => ({ size, data: png[`tile-${size}`] }))));
writeFileSync(join(ICONS, "128x128.png"), png["tile-128"]);
writeFileSync(join(ICONS, "128x128@2x.png"), png["tile-256"]);
writeFileSync(join(ICONS, "icon.png"), png["tile-512"]);
// tray-<state>-<light|dark|colour>.ico, read by platform/tray.rs.
for (const state of STATES) {
  for (const style of STYLES) {
    const frames = GLYPH_SIZES.map((size) => ({ size, data: png[`${state}-${style}-${size}`] }));
    writeFileSync(join(ICONS, `tray-${state}-${style}.ico`), ico(frames));
  }
}
writeFileSync(join(ROOT, "app-icon.png"), png["tile-1024"]);
writeFileSync(join(ROOT, "assets", "icon.svg"), tile(null) + "\n");
if (process.env.WITNESS_ICON_PREVIEW) {
  for (const [name, data] of Object.entries(png)) writeFileSync(join(process.env.WITNESS_ICON_PREVIEW, `${name}.png`), data);
}
console.log("Wrote crates/witness/icons/*, app-icon.png and assets/icon.svg");
