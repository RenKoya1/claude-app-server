#!/usr/bin/env node
/**
 * postinstall:
 *   1. Ensure dist/bin/<platform>/claude-app-server is present, downloading
 *      from GitHub Releases on first install.
 *   2. Prune the Claude Agent SDK's bundled `ripgrep` vendor directories to
 *      keep only the one matching the current platform, freeing ~40MB on
 *      disk for users.
 *
 * Behavior:
 *   - If `CLAUDE_APP_SERVER_SKIP_DOWNLOAD=1`, skip step 1.
 *   - If the binary already exists (e.g. local dev where build-local.sh ran),
 *     skip step 1.
 *   - Step 2 always runs unless `CLAUDE_APP_SERVER_KEEP_VENDOR=1`.
 */

import { existsSync, mkdirSync, createWriteStream, chmodSync, rmSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { platform, arch } from "node:os";
import { pipeline } from "node:stream/promises";
import { spawn } from "node:child_process";
import https from "node:https";

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

async function downloadTarball(url, destFile) {
  return new Promise((resolve, reject) => {
    const file = createWriteStream(destFile);
    const req = https.get(url, { headers: { "User-Agent": "claude-app-server-installer" } }, (res) => {
      if (res.statusCode && res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        // follow redirect
        downloadTarball(res.headers.location, destFile).then(resolve, reject);
        res.resume();
        return;
      }
      if (res.statusCode !== 200) {
        reject(new Error(`download failed: status=${res.statusCode} url=${url}`));
        res.resume();
        return;
      }
      pipeline(res, file).then(resolve, reject);
    });
    req.on("error", reject);
  });
}

async function extractTarGz(tarball, destDir) {
  await new Promise((resolve, reject) => {
    const child = spawn("tar", ["-xzf", tarball, "-C", destDir], { stdio: "inherit" });
    child.on("exit", (code) => (code === 0 ? resolve(null) : reject(new Error(`tar exit ${code}`))));
    child.on("error", reject);
  });
}

// Map our platform triple onto the SDK's ripgrep vendor directory name.
// The SDK ships dirs like `arm64-darwin`, `x64-linux`, etc.
function ripgrepVendorName(triple) {
  switch (triple) {
    case "darwin-arm64": return "arm64-darwin";
    case "darwin-x64":   return "x64-darwin";
    case "linux-x64":    return "x64-linux";
    case "linux-arm64":  return "arm64-linux";
    case "win32-x64":    return "x64-win32";
    default:             return null;
  }
}

function pruneRipgrepVendor(triple) {
  if (process.env.CLAUDE_APP_SERVER_KEEP_VENDOR === "1") return;
  const keep = ripgrepVendorName(triple);
  if (!keep) return;
  const vendorRoot = join(
    PKG_ROOT,
    "dist",
    "sidecar",
    "node_modules",
    "@anthropic-ai",
    "claude-agent-sdk",
    "vendor",
    "ripgrep",
  );
  if (!existsSync(vendorRoot)) return;
  let entries;
  try {
    entries = readdirSync(vendorRoot);
  } catch {
    return;
  }
  let freed = 0;
  for (const entry of entries) {
    if (entry === keep) continue;
    const full = join(vendorRoot, entry);
    try {
      const st = statSync(full);
      if (st.isDirectory()) {
        freed += dirSize(full);
        rmSync(full, { recursive: true, force: true });
      }
    } catch {}
  }
  if (freed > 0) {
    console.log(`postinstall: pruned ripgrep vendor (kept ${keep}, freed ${(freed / (1024 * 1024)).toFixed(1)} MB)`);
  }
}

function dirSize(p) {
  let total = 0;
  try {
    for (const entry of readdirSync(p, { withFileTypes: true })) {
      const full = join(p, entry.name);
      if (entry.isDirectory()) total += dirSize(full);
      else if (entry.isFile()) {
        try { total += statSync(full).size; } catch {}
      }
    }
  } catch {}
  return total;
}

async function ensureBinary(triple) {
  if (process.env.CLAUDE_APP_SERVER_SKIP_DOWNLOAD === "1") {
    console.log("postinstall: binary download skipped (CLAUDE_APP_SERVER_SKIP_DOWNLOAD=1)");
    return;
  }
  const ext = platform() === "win32" ? ".exe" : "";
  const binaryPath = join(PKG_ROOT, "dist", "bin", triple, `claude-app-server${ext}`);
  if (existsSync(binaryPath)) {
    console.log(`postinstall: binary already present at ${binaryPath}`);
    return;
  }

  const tpl =
    process.env.CLAUDE_APP_SERVER_RELEASE_URL ||
    "https://github.com/RenKoya1/claude-app-server/releases/download/v{version}/claude-app-server-{triple}.tar.gz";

  const pkg = JSON.parse(
    await import("node:fs/promises").then((m) => m.readFile(join(PKG_ROOT, "package.json"), "utf8")),
  );
  const url = tpl.replace("{version}", pkg.version).replace("{triple}", triple);

  const destDir = join(PKG_ROOT, "dist", "bin", triple);
  mkdirSync(destDir, { recursive: true });
  const tarball = join(destDir, "binary.tar.gz");
  console.log(`postinstall: downloading ${url}`);
  await downloadTarball(url, tarball);
  console.log(`postinstall: extracting`);
  await extractTarGz(tarball, destDir);
  if (existsSync(binaryPath)) {
    chmodSync(binaryPath, 0o755);
    try { rmSync(tarball); } catch {}
    console.log(`postinstall: installed ${binaryPath}`);
  } else {
    console.warn(`postinstall: tarball did not contain ${binaryPath}`);
  }
}

async function main() {
  let triple;
  try {
    triple = platformTriple();
  } catch (e) {
    console.warn(`postinstall: ${e.message}; binary download skipped.`);
    return;
  }

  await ensureBinary(triple);
  pruneRipgrepVendor(triple);
}

main().catch((e) => {
  console.warn(`postinstall failed: ${e.message}`);
  // Do not fail the install.
});
