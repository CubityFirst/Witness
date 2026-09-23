import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));

const base = readJson("src-tauri/tauri.conf.json");
const release = readJson("src-tauri/tauri.release.conf.json");
const updaterOverlay = readJson("src-tauri/tauri.updater.conf.json");
const expectedResources = {
  "target/release/onnxruntime_providers_cuda.dll": "",
  "target/release/onnxruntime_providers_shared.dll": "",
};

assert.equal(
  base.bundle.active,
  false,
  "base Tauri config must not bundle generated runtime DLLs during Cargo builds",
);
assert.equal(
  Object.hasOwn(base.bundle, "resources"),
  false,
  "generated runtime resources belong only in the release overlay",
);
assert.equal(
  release.bundle.active,
  true,
  "release overlay must enable installer bundling",
);
assert.deepEqual(
  release.bundle.resources,
  expectedResources,
  "release overlay must package the exact ONNX Runtime provider DLL set",
);

assert.equal(
  release.bundle.windows?.nsis?.installMode,
  "currentUser",
  "installer must be per-user so in-app updates never need elevation",
);
assert.equal(
  Object.hasOwn(release.bundle, "createUpdaterArtifacts"),
  false,
  "updater signing belongs only in the signed-release overlay (unsigned CI builds must still bundle)",
);
assert.deepEqual(
  updaterOverlay,
  { bundle: { createUpdaterArtifacts: true } },
  "signed-release overlay must only enable updater artifacts",
);
const updater = base.plugins?.updater;
assert.ok(updater?.pubkey, "updater public key must be configured");
assert.deepEqual(
  updater.endpoints,
  ["https://github.com/CubityFirst/Witness/releases/latest/download/latest.json"],
  "updater must read the latest GitHub release feed",
);
const cargoVersion = readFileSync("src-tauri/Cargo.toml", "utf8").match(/^version = "([^"]+)"/m)?.[1];
const npmVersion = readJson("package.json").version;
assert.equal(cargoVersion, base.version, "Cargo.toml and tauri.conf.json versions must match");
assert.equal(npmVersion, base.version, "package.json and tauri.conf.json versions must match");

console.log("Windows release config defers and verifies 2 generated runtime DLLs.");
