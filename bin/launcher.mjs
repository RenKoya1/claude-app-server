#!/usr/bin/env node
/**
 * Entry point for `npx claude-app-server` / `claude-app-server`.
 *
 * Locates the bundled Rust binary and TypeScript sidecar, then execs the
 * Rust binary with `CLAUDE_APP_SERVER_SIDECAR` pointing at the sidecar so
 * it can spawn it on demand.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { platform, arch } from "node:os";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = join(HERE, "..");

function platformTriple() {
  const p = platform();
  const a = arch();
  if (p === "darwin" && a === "arm64") return "darwin-arm64";
  if (p === "darwin" && a === "x64") return "darwin-x64";
  if (p === "linux" && a === "x64") return "linux-x64";
  if (p === "linux" && a === "arm64") return "linux-arm64";
  if (p === "win32" && a === "x64") return "win32-x64";
  throw new Error(`unsupported platform ${p}-${a}`);
}

function findBinary() {
  const triple = platformTriple();
  const ext = platform() === "win32" ? ".exe" : "";
  const candidates = [
    join(PKG_ROOT, "dist", "bin", triple, `claude-app-server${ext}`),
    join(PKG_ROOT, "target", "release", `claude-app-server${ext}`),
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  throw new Error(
    `claude-app-server binary not found. Tried:\n  ${candidates.join("\n  ")}\n` +
      `Run \`npm run build\` from the package root, or reinstall the package so the postinstall script downloads the binary.`,
  );
}

function findSidecar() {
  const candidates = [
    join(PKG_ROOT, "dist", "sidecar", "index.js"),
    join(PKG_ROOT, "sidecar", "dist", "index.js"),
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  throw new Error(
    `sidecar not found. Tried:\n  ${candidates.join("\n  ")}\n` +
      `Run \`npm run build\` from the package root.`,
  );
}

function main() {
  let binary;
  let sidecar;
  try {
    binary = findBinary();
    sidecar = findSidecar();
  } catch (e) {
    process.stderr.write(`${e.message}\n`);
    process.exit(127);
  }

  const env = { ...process.env, CLAUDE_APP_SERVER_SIDECAR: sidecar };
  const child = spawn(binary, process.argv.slice(2), {
    stdio: "inherit",
    env,
  });
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    process.exit(code ?? 0);
  });
  child.on("error", (err) => {
    process.stderr.write(`launcher: failed to spawn ${binary}: ${err.message}\n`);
    process.exit(126);
  });

  // Forward signals so SIGINT/SIGTERM stop the child cleanly.
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => {
      child.kill(sig);
    });
  }
}

main();
