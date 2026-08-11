import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));

const base = readJson("src-tauri/tauri.conf.json");
const release = readJson("src-tauri/tauri.release.conf.json");
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

console.log("Windows release config defers and verifies 2 generated runtime DLLs.");
