#!/usr/bin/env node
// Runs the native git-carve binary from the platform package npm installed next to this one
// (DECISIONS.md D39). No install scripts, no downloads: npm's own optionalDependencies pick
// the one package whose `os` and `cpu` match.
"use strict";

const { spawnSync } = require("child_process");
const path = require("path");

const platforms = {
  "linux x64": "@git-carve/linux-x64",
  "linux arm64": "@git-carve/linux-arm64",
  "darwin x64": "@git-carve/darwin-x64",
  "darwin arm64": "@git-carve/darwin-arm64",
};

const key = `${process.platform} ${process.arch}`;
if (process.platform === "win32") {
  console.error("git carve: Windows is not supported yet. Run it inside WSL, where it is supported.");
  process.exit(1);
}
const pkg = platforms[key];
if (!pkg) {
  console.error(`git carve: no prebuilt binary for ${key}. Build from source: cargo install git-carve`);
  process.exit(1);
}

let binary;
try {
  binary = path.join(path.dirname(require.resolve(`${pkg}/package.json`)), "bin", "git-carve");
} catch {
  console.error(
    `git carve: the platform package ${pkg} is missing. Reinstall without --no-optional ` +
      "(npm install -g git-carve)."
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`git carve: could not run ${binary}: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
