// Bumps the app version everywhere the build reads it, so a release tag, the
// installer name and the updater's version comparison can never disagree.
// Usage: npm run version:set -- 0.2.0
import { readFileSync, writeFileSync } from "node:fs";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  console.error("usage: npm run version:set -- <major.minor.patch>");
  process.exit(1);
}

function edit(path, pattern, label) {
  const source = readFileSync(path, "utf8");
  if (!pattern.test(source)) throw new Error(`Could not find the ${label} in ${path}`);
  writeFileSync(path, source.replace(pattern, (_, before) => `${before}${version}"`));
}

edit("package.json", /("version": ")[^"]+"/, "package version");
edit("src-tauri/tauri.conf.json", /("version": ")[^"]+"/, "app version");
edit("src-tauri/Cargo.toml", /^(version = ")[^"]+"/m, "crate version");
edit("src-tauri/Cargo.lock", /(\[\[package\]\]\r?\nname = "witness"\r?\nversion = ")[^"]+"/, "lockfile entry");
// The npm lockfile carries the version twice (root + packages[""]).
const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
lock.version = version;
lock.packages[""].version = version;
writeFileSync("package-lock.json", `${JSON.stringify(lock, null, 2)}\n`);

console.log(`Witness version set to ${version}. Commit, then tag v${version} and push the tag.`);
