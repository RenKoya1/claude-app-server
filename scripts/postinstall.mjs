#!/usr/bin/env node
/**
 * postinstall: ensure dist/bin/<platform>/claude-app-server is present after
 * `npm install`.
 *
 * Behavior:
 *   - If `CLAUDE_APP_SERVER_SKIP_DOWNLOAD=1`, do nothing.
 *   - If the binary already exists (e.g. local dev where build-local.sh ran),
 *     do nothing.
 *   - Otherwise, download a release tarball from GitHub Releases for the
 *     current platform and extract it into dist/bin/<triple>/.
 *
 * The download URL is templated from `release.binaryUrlTemplate` in
 * package.json so the same script works pre- and post-publish.
 */

import { existsSync, mkdirSync, createWriteStream, chmodSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { platform, arch } from "node:os";
import { pipeline } from "node:stream/promises";
import { createGunzip } from "node:zlib";
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

async function main() {
  if (process.env.CLAUDE_APP_SERVER_SKIP_DOWNLOAD === "1") {
    console.log("postinstall: skipped (CLAUDE_APP_SERVER_SKIP_DOWNLOAD=1)");
    return;
  }
  let triple;
  try {
    triple = platformTriple();
  } catch (e) {
    console.warn(`postinstall: ${e.message}; binary download skipped.`);
    return;
  }

  const ext = platform() === "win32" ? ".exe" : "";
  const binaryPath = join(PKG_ROOT, "dist", "bin", triple, `claude-app-server${ext}`);
  if (existsSync(binaryPath)) {
    console.log(`postinstall: binary already present at ${binaryPath}`);
    return;
  }

  // We have not published releases yet, so postinstall just logs and exits
  // cleanly. When releases exist, set CLAUDE_APP_SERVER_RELEASE_URL or
  // expand pkg.release.binaryUrlTemplate to point at the tarball.
  const tpl =
    process.env.CLAUDE_APP_SERVER_RELEASE_URL ||
    "https://github.com/example/claude-app-server/releases/download/v{version}/claude-app-server-{triple}.tar.gz";
  if (tpl.includes("example/claude-app-server")) {
    console.warn(
      "postinstall: no release URL configured. Run `npm run build` from the package root for local dev, or set CLAUDE_APP_SERVER_RELEASE_URL.",
    );
    return;
  }

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
    console.log(`postinstall: installed ${binaryPath}`);
  } else {
    console.warn(`postinstall: tarball did not contain ${binaryPath}`);
  }
}

main().catch((e) => {
  console.warn(`postinstall failed: ${e.message}`);
  // Do not fail the install.
});
