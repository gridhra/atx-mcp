#!/usr/bin/env node
// Assembles the publishable npm package tree into npm/dist/ from the raw
// binaries produced by the release workflow.
//
//   node npm/scripts/build-packages.mjs --version 0.1.0 --bins npmbins
//
// `--bins` points at a directory laid out the way actions/download-artifact
// leaves it:  <bins>/npmbin-<slug>/atx-mcp[.exe]
//
// Output:
//   npm/dist/atx-mcp/                 (launcher, optionalDependencies pinned)
//   npm/dist/atx-mcp-<slug>/          (one per platform, binary only)

import { mkdirSync, rmSync, cpSync, readFileSync, writeFileSync, existsSync, chmodSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const NPM_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const REPO_ROOT = resolve(NPM_DIR, "..");
const LICENSE = join(REPO_ROOT, "LICENSE");
const MAIN_SRC = join(NPM_DIR, "atx-mcp");
const DIST = join(NPM_DIR, "dist");

const PLATFORMS = [
  { slug: "darwin-arm64", os: "darwin", cpu: "arm64", exe: "atx-mcp" },
  { slug: "darwin-x64", os: "darwin", cpu: "x64", exe: "atx-mcp" },
  { slug: "linux-x64", os: "linux", cpu: "x64", exe: "atx-mcp" },
  { slug: "linux-arm64", os: "linux", cpu: "arm64", exe: "atx-mcp" },
  { slug: "win32-x64", os: "win32", cpu: "x64", exe: "atx-mcp.exe" },
];

function arg(name, required = true) {
  const i = process.argv.indexOf(`--${name}`);
  if (i === -1 || !process.argv[i + 1]) {
    if (required) {
      console.error(`missing required argument --${name}`);
      process.exit(2);
    }
    return undefined;
  }
  return process.argv[i + 1];
}

const version = arg("version").replace(/^v/, "");
const binsDir = resolve(arg("bins"));
const allowMissing = process.argv.includes("--allow-missing");

if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`--version must be a semver string, got: ${version}`);
  process.exit(2);
}

// gridhra/atx-mcp is the real repository, but npm provenance requires the
// package's repository URL to match the building repo exactly (matters for
// forks or a future rename), so substitute it from CI's own view when
// available rather than hardcoding the assumption.
const realRepo = process.env.GITHUB_REPOSITORY;
const fixOwner = (s) =>
  realRepo ? s.replaceAll("gridhra/atx-mcp", realRepo) : s;

rmSync(DIST, { recursive: true, force: true });
mkdirSync(DIST, { recursive: true });

const optionalDependencies = {};
const built = [];

for (const p of PLATFORMS) {
  const name = `atx-mcp-${p.slug}`;
  optionalDependencies[name] = version;

  const src = join(binsDir, `npmbin-${p.slug}`, p.exe);
  if (!existsSync(src)) {
    if (!allowMissing) {
      console.error(`missing binary for ${p.slug}: ${src}`);
      process.exit(1);
    }
    console.warn(`! skipping ${p.slug} (no binary at ${src})`);
    continue;
  }

  const out = join(DIST, name);
  mkdirSync(join(out, "bin"), { recursive: true });
  cpSync(src, join(out, "bin", p.exe));
  cpSync(LICENSE, join(out, "LICENSE"));
  if (p.os !== "win32") chmodSync(join(out, "bin", p.exe), 0o755);

  writeFileSync(
    join(out, "package.json"),
    JSON.stringify(
      {
        name,
        version,
        description: `Prebuilt atx-mcp binary for ${p.os} ${p.cpu}.`,
        license: "MIT",
        repository: {
          type: "git",
          url: fixOwner("git+https://github.com/gridhra/atx-mcp.git"),
        },
        os: [p.os],
        cpu: [p.cpu],
        files: [`bin/${p.exe}`, "LICENSE"],
        preferUnplugged: true,
      },
      null,
      2
    ) + "\n"
  );
  built.push(name);
  console.log(`  built ${name}@${version}`);
}

// Main launcher package.
const mainOut = join(DIST, "atx-mcp");
mkdirSync(join(mainOut, "bin"), { recursive: true });
writeFileSync(
  join(mainOut, "bin", "atx-mcp.js"),
  fixOwner(readFileSync(join(MAIN_SRC, "bin", "atx-mcp.js"), "utf8"))
);
chmodSync(join(mainOut, "bin", "atx-mcp.js"), 0o755);
writeFileSync(
  join(mainOut, "README.md"),
  fixOwner(readFileSync(join(MAIN_SRC, "README.md"), "utf8"))
);
cpSync(LICENSE, join(mainOut, "LICENSE"));

const mainPkg = JSON.parse(fixOwner(readFileSync(join(MAIN_SRC, "package.json"), "utf8")));
mainPkg.version = version;
mainPkg.optionalDependencies = optionalDependencies;
writeFileSync(join(mainOut, "package.json"), JSON.stringify(mainPkg, null, 2) + "\n");

console.log(`  built atx-mcp@${version} (${built.length}/${PLATFORMS.length} platform packages)`);
console.log(`\nOutput: ${DIST}`);
