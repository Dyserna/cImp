#!/usr/bin/env node
// Sync the cimp version across the three places that need to agree:
//   - package.json                  (drives the Settings → About display)
//   - src-tauri/Cargo.toml          (Rust crate version)
//   - src-tauri/tauri.conf.json     (Tauri runtime version)
// Then refresh Cargo.lock so the workspace is consistent.
//
// The CI workflow asserts these three match the git tag, so running this
// before tagging is the supported release flow. See docs/RELEASE.md.

import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const repo = dirname(dirname(fileURLToPath(import.meta.url)));

const target = process.argv[2];
if (!target) {
  console.error('usage: node scripts/bump-version.mjs <X.Y.Z>');
  process.exit(2);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(target)) {
  console.error(`error: "${target}" is not a valid semver`);
  process.exit(2);
}

const pkgPath = join(repo, 'package.json');
const lockPath = join(repo, 'package-lock.json');
const cargoPath = join(repo, 'src-tauri', 'Cargo.toml');
const confPath = join(repo, 'src-tauri', 'tauri.conf.json');

for (const p of [pkgPath, lockPath, cargoPath, confPath]) {
  if (!existsSync(p)) {
    console.error(`error: missing ${p}`);
    process.exit(1);
  }
}

// package.json — preserve formatting via JSON round-trip + trailing newline.
const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
const prevPkg = pkg.version;
pkg.version = target;
writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n');

// package-lock.json — root version appears twice: top-level and inside
// packages[""]. `npm ci` is strict about this matching package.json, so
// touching both is non-optional.
const lock = JSON.parse(readFileSync(lockPath, 'utf8'));
const prevLock = lock.version;
lock.version = target;
if (lock.packages && lock.packages['']) {
  lock.packages[''].version = target;
}
writeFileSync(lockPath, JSON.stringify(lock, null, 2) + '\n');

// Cargo.toml — only the [package] version line. We avoid a full TOML parse
// dep; the regex anchors on `^version = "..."` inside [package].
let cargo = readFileSync(cargoPath, 'utf8');
const cargoRe = /(\[package\][\s\S]*?\nversion\s*=\s*")([^"]+)(")/;
const cargoMatch = cargo.match(cargoRe);
if (!cargoMatch) {
  console.error('error: could not locate [package] version in Cargo.toml');
  process.exit(1);
}
const prevCargo = cargoMatch[2];
cargo = cargo.replace(cargoRe, `$1${target}$3`);
writeFileSync(cargoPath, cargo);

// tauri.conf.json — JSON, top-level "version" field.
const conf = JSON.parse(readFileSync(confPath, 'utf8'));
const prevConf = conf.version;
conf.version = target;
writeFileSync(confPath, JSON.stringify(conf, null, 2) + '\n');

// Refresh Cargo.lock so cimp's own entry matches. `cargo update -p cimp`
// only touches our package, leaving every other dep at its current pin.
try {
  execFileSync('cargo', ['update', '-p', 'cimp'], {
    cwd: join(repo, 'src-tauri'),
    stdio: 'inherit',
  });
} catch {
  console.error('error: cargo update failed — make sure cargo is on PATH');
  process.exit(1);
}

console.log(`\nbumped:`);
console.log(`  package.json              ${prevPkg} -> ${target}`);
console.log(`  package-lock.json         ${prevLock} -> ${target}`);
console.log(`  src-tauri/Cargo.toml      ${prevCargo} -> ${target}`);
console.log(`  src-tauri/tauri.conf.json ${prevConf} -> ${target}`);
console.log(`\nnext:`);
console.log(`  git add package.json package-lock.json src-tauri/Cargo.toml src-tauri/tauri.conf.json src-tauri/Cargo.lock`);
console.log(`  git commit -m "Release v${target}"`);
console.log(`  git tag v${target}`);
console.log(`  git push && git push --tags`);
