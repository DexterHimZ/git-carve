#!/usr/bin/env node
// Assembles the npm packages (DECISIONS.md D39) from built binaries, without publishing.
//   node scripts/npm-pack.mjs <out-dir> <rust-target>=<path-to-git-carve-binary>...
// Writes <out-dir>/git-carve (the shim, versioned from Cargo.toml) and one
// <out-dir>/<platform> package per binary. The license comes from Cargo.toml too.
import { copyFileSync, chmodSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
const field = (name) => cargo.match(new RegExp(`^${name}\\s*=\\s*"([^"]*)"`, "m"))?.[1];
const version = field("version");
const license = field("license") ?? "UNLICENSED";
const targets = {
  "x86_64-unknown-linux-musl": ["linux", "x64"],
  "aarch64-unknown-linux-musl": ["linux", "arm64"],
  "x86_64-apple-darwin": ["darwin", "x64"],
  "aarch64-apple-darwin": ["darwin", "arm64"],
};

const [out, ...builds] = process.argv.slice(2);
if (!out || builds.length === 0) {
  console.error("usage: npm-pack.mjs <out-dir> <rust-target>=<binary>...");
  process.exit(2);
}

const shim = JSON.parse(readFileSync(join(root, "npm/git-carve/package.json"), "utf8"));
shim.version = version;
shim.license = license;
for (const dep of Object.keys(shim.optionalDependencies)) shim.optionalDependencies[dep] = version;
const shimDir = join(out, "git-carve");
mkdirSync(join(shimDir, "bin"), { recursive: true });
writeFileSync(join(shimDir, "package.json"), JSON.stringify(shim, null, 2) + "\n");
copyFileSync(join(root, "npm/git-carve/bin/git-carve.js"), join(shimDir, "bin/git-carve.js"));
chmodSync(join(shimDir, "bin/git-carve.js"), 0o755);
copyFileSync(join(root, "README.md"), join(shimDir, "README.md"));

for (const build of builds) {
  const [target, binary] = build.split("=");
  const platform = targets[target];
  if (!platform || !binary) {
    console.error(`unknown target or missing binary: ${build}`);
    process.exit(2);
  }
  const [os, cpu] = platform;
  const name = `@git-carve/${os}-${cpu}`;
  if (!(name in shim.optionalDependencies)) {
    console.error(`${name} is not an optional dependency of the shim`);
    process.exit(2);
  }
  const dir = join(out, `${os}-${cpu}`);
  mkdirSync(join(dir, "bin"), { recursive: true });
  copyFileSync(binary, join(dir, "bin/git-carve"));
  chmodSync(join(dir, "bin/git-carve"), 0o755);
  const pkg = {
    name,
    version,
    description: `The git-carve binary for ${os} ${cpu}. Install git-carve instead.`,
    license,
    os: [os],
    cpu: [cpu],
    files: ["bin/git-carve"],
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  console.log(`${name}@${version} <- ${binary}`);
}
console.log(`git-carve@${version} -> ${shimDir}`);
