import { readFileSync } from "node:fs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");

function capture(source, pattern, label) {
  const match = source.match(pattern);
  if (!match?.groups?.body) {
    throw new Error(`Could not find ${label}`);
  }
  return match.groups.body;
}

function quotedValues(source) {
  return [...source.matchAll(/"([a-z0-9_-]+)"/g)].map((match) => match[1]);
}

function assertSame(label, actual, expected) {
  const normalizedActual = [...actual].sort();
  const normalizedExpected = [...expected].sort();
  if (new Set(actual).size !== actual.length) {
    throw new Error(`${label} contains duplicate entries`);
  }
  if (JSON.stringify(normalizedActual) !== JSON.stringify(normalizedExpected)) {
    const missing = normalizedExpected.filter((item) => !actual.includes(item));
    const extra = normalizedActual.filter((item) => !expected.includes(item));
    throw new Error(
      `${label} is out of sync (missing: ${missing.join(", ") || "none"}; extra: ${extra.join(", ") || "none"})`,
    );
  }
}

const mainSource = read("src-tauri/src/main.rs");
const handlerBody = capture(
  mainSource,
  /\.invoke_handler\(tauri::generate_handler!\[(?<body>[\s\S]*?)\]\)/,
  "the Tauri invoke handler",
);
const handledCommands = [...handlerBody.matchAll(/(?:commands|updater)::([a-z0-9_]+)/g)].map(
  (match) => match[1],
);

const buildSource = read("src-tauri/build.rs");
const manifestBody = capture(
  buildSource,
  /const COMMANDS:.*?=\s*&\[(?<body>[\s\S]*?)\];/,
  "the build-script command manifest",
);
const manifestCommands = quotedValues(manifestBody);

const permissionSource = read("src-tauri/permissions/main.toml");
const permissionBody = capture(
  permissionSource,
  /permissions\s*=\s*\[(?<body>[\s\S]*?)\]/,
  "the main-window command permission set",
);
const permittedCommands = quotedValues(permissionBody).map((permission) =>
  permission.replace(/^allow-/, "").replaceAll("-", "_"),
);

assertSame("build-script commands", manifestCommands, handledCommands);
assertSame("main-window command permissions", permittedCommands, handledCommands);

const mainCapability = JSON.parse(read("src-tauri/capabilities/default.json"));
const captionsCapability = JSON.parse(read("src-tauri/capabilities/captions.json"));
const tauriConfig = JSON.parse(read("src-tauri/tauri.conf.json"));

assertSame("main capability windows", mainCapability.windows, ["main"]);
assertSame("captions capability windows", captionsCapability.windows, ["captions"]);
assertSame("enabled capabilities", tauriConfig.app.security.capabilities, ["main", "captions"]);
assertSame("captions permissions", captionsCapability.permissions, [
  "core:event:allow-listen",
  "core:event:allow-unlisten",
  "core:window:allow-start-dragging",
  "core:window:allow-close",
]);

if (mainCapability.permissions.includes("core:default")) {
  throw new Error("The main capability must grant individual core permissions");
}

console.log(`Tauri ACL is synchronized for ${handledCommands.length} application commands.`);
