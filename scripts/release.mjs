#!/usr/bin/env node
/**
 * One-command release driver.
 *
 *   npm run release           # interactive (defaults to patch)
 *   npm run release patch     # bump 0.1.0 -> 0.1.1
 *   npm run release minor     # bump 0.1.0 -> 0.2.0
 *   npm run release major     # bump 0.1.0 -> 1.0.0
 *
 * What it does locally:
 *   1. Verify the working tree is clean and on a release-eligible branch.
 *   2. Verify `gh` is authenticated (needed for the CI workflow).
 *   3. `npm version <bump>` — bumps package.json and creates a git tag.
 *   4. Push the branch + tag to origin.
 *
 * After the tag lands on GitHub, `.github/workflows/release.yml` takes over:
 *   - cross-builds the Rust binary for every supported platform,
 *   - bundles `dist/sidecar` once from the TS source,
 *   - downloads all matrix artifacts onto a single runner,
 *   - publishes `@renkoya1/claude-app-server` to npm and creates the GH
 *     Release with the platform tarballs attached.
 *
 * If you want to publish strictly from this machine (no CI), use
 *   --local
 * which builds only the current platform, then `npm publish` directly.
 * That ships a single-platform package; useful for dogfooding only.
 */

import { spawnSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = join(HERE, "..");

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { stdio: "inherit", cwd: PKG_ROOT, ...opts });
  if (r.status !== 0) {
    throw new Error(`command failed: ${cmd} ${args.join(" ")} (exit ${r.status})`);
  }
}

function runCapture(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { cwd: PKG_ROOT, encoding: "utf8", ...opts });
  return { status: r.status, stdout: (r.stdout ?? "").trim(), stderr: (r.stderr ?? "").trim() };
}

function bail(msg) {
  process.stderr.write(`release: ${msg}\n`);
  process.exit(1);
}

function ensureCleanGit() {
  const status = runCapture("git", ["status", "--porcelain"]);
  if (status.status !== 0) bail("git status failed; is this a git repo?");
  if (status.stdout) bail(`working tree is not clean:\n${status.stdout}\n  commit or stash first.`);
}

function currentBranch() {
  const r = runCapture("git", ["rev-parse", "--abbrev-ref", "HEAD"]);
  if (r.status !== 0) bail("could not read current git branch");
  return r.stdout;
}

function ensureGhAuth() {
  const r = runCapture("gh", ["auth", "status"]);
  if (r.status !== 0) bail("gh CLI is not authenticated; run `gh auth login` first.");
}

function ensureNpmAuthForLocalPublish() {
  const r = runCapture("npm", ["whoami"]);
  if (r.status !== 0) bail("npm is not logged in; run `npm login` before --local.");
}

function readPkg() {
  return JSON.parse(readFileSync(join(PKG_ROOT, "package.json"), "utf8"));
}

function parseArgs(argv) {
  const args = { bump: "patch", local: false, dryRun: false, skipSmoke: false };
  for (const arg of argv) {
    if (arg === "patch" || arg === "minor" || arg === "major") args.bump = arg;
    else if (arg === "--local") args.local = true;
    else if (arg === "--dry-run") args.dryRun = true;
    else if (arg === "--skip-smoke") args.skipSmoke = true;
    else if (arg === "--help" || arg === "-h") {
      console.log(readFileSync(import.meta.url ? fileURLToPath(import.meta.url) : "release.mjs", "utf8").split("\n").slice(0, 30).join("\n"));
      process.exit(0);
    } else if (/^v?\d+\.\d+\.\d+/.test(arg)) {
      args.bump = arg.replace(/^v/, "");
    } else {
      bail(`unknown argument: ${arg}`);
    }
  }
  return args;
}

function ensureSmokeTestPasses() {
  console.log("release: building local artifacts (cargo + tsc + stage)");
  run("./scripts/build-local.sh", []);

  console.log("release: running smoke test against freshly-built launcher");
  const smokeInput = [
    '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"release-smoke","version":"0.0.0"}}}',
    '{"method":"initialized","params":{}}',
    '{"id":2,"method":"config/read"}',
    '{"id":3,"method":"fs/createDirectory","params":{"path":"/tmp/cas_release_smoke"}}',
    '{"id":4,"method":"fs/writeFile","params":{"path":"/tmp/cas_release_smoke/x","dataBase64":"b2s="}}',
    '{"id":5,"method":"fs/readFile","params":{"path":"/tmp/cas_release_smoke/x"}}',
    '{"id":6,"method":"command/exec","params":{"command":["echo","ok"]}}',
    '{"id":7,"method":"thread/start","params":{"model":"claude-haiku-4-5-20251001"}}',
    '{"id":8,"method":"thread/loaded/list"}',
    '{"id":9,"method":"fs/remove","params":{"path":"/tmp/cas_release_smoke"}}',
    "",
  ].join("\n");

  const r = spawnSync("node", ["bin/launcher.mjs"], {
    cwd: PKG_ROOT,
    input: smokeInput,
    encoding: "utf8",
    timeout: 30_000,
  });
  if (r.status !== 0) {
    bail(`smoke test launcher exited with ${r.status}\nstderr:\n${r.stderr}`);
  }
  const errCount = (r.stdout.match(/"error":\s*{/g) ?? []).length;
  if (errCount > 0) {
    bail(`smoke test produced ${errCount} JSON-RPC error response(s):\n${r.stdout}`);
  }
  for (const id of [1, 2, 3, 4, 5, 6, 7, 8, 9]) {
    if (!r.stdout.includes(`"id":${id},"result"`)) {
      bail(`smoke test missing response for id=${id}:\n${r.stdout}`);
    }
  }
  console.log("release: smoke test passed (9/9 ids responded with result)");
}

function ensurePackageSize() {
  console.log("release: checking npm pack size budget");
  const r = spawnSync(
    "npm",
    ["pack", "--dry-run", "--json"],
    { cwd: PKG_ROOT, encoding: "utf8", env: { ...process.env, CLAUDE_APP_SERVER_SKIP_DOWNLOAD: "1" } },
  );
  if (r.status !== 0) {
    bail(`npm pack --dry-run failed:\n${r.stderr}`);
  }
  let pack;
  try {
    const arr = JSON.parse(r.stdout);
    pack = Array.isArray(arr) ? arr[0] : arr;
  } catch (e) {
    bail(`could not parse npm pack output: ${e.message}\n${r.stdout}`);
  }
  const tarballBytes = pack.size ?? 0;
  const unpackedBytes = pack.unpackedSize ?? 0;
  // Floor is set by the Claude Agent SDK's per-platform ripgrep vendor
  // binaries (~40 MB across darwin x2 + linux x2). The user-side
  // postinstall prunes 3 of 4 to bring on-disk size to ~50 MB.
  const TARBALL_CAP = 40 * 1024 * 1024;
  const UNPACKED_CAP = 100 * 1024 * 1024;
  console.log(
    `release: package size ${(tarballBytes / 1024 / 1024).toFixed(1)} MB tarball, ${(unpackedBytes / 1024 / 1024).toFixed(1)} MB unpacked`,
  );
  if (tarballBytes > TARBALL_CAP) {
    bail(
      `tarball size ${(tarballBytes / 1024 / 1024).toFixed(1)} MB exceeds cap ${(TARBALL_CAP / 1024 / 1024).toFixed(0)} MB`,
    );
  }
  if (unpackedBytes > UNPACKED_CAP) {
    bail(
      `unpacked size ${(unpackedBytes / 1024 / 1024).toFixed(1)} MB exceeds cap ${(UNPACKED_CAP / 1024 / 1024).toFixed(0)} MB`,
    );
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const pkg = readPkg();

  console.log(`release: package ${pkg.name}@${pkg.version}`);
  console.log(`release: bump=${args.bump} local=${args.local} dryRun=${args.dryRun}`);

  ensureCleanGit();
  const branch = currentBranch();
  console.log(`release: on branch ${branch}`);

  // CLAUDE.md hard rule: never publish without smoke test + size check.
  // Both gates can be disabled with --skip-smoke for emergency hotfixes
  // (but think twice before doing that).
  if (!args.skipSmoke) {
    ensureSmokeTestPasses();
    ensurePackageSize();
  } else {
    console.warn("release: --skip-smoke set; CLAUDE.md hard rule bypassed (use only for hotfixes).");
  }

  if (args.local) {
    ensureNpmAuthForLocalPublish();
    console.log("release: --local; building current platform and publishing from here.");
    if (args.dryRun) {
      console.log("release: dry-run, skipping build + publish.");
      return;
    }
    // Bump version (creates a tag).
    run("npm", ["version", args.bump, "-m", `chore(release): %s`]);
    // Build local artifacts.
    run("./scripts/build-local.sh", []);
    // Publish to npm.
    run("npm", ["publish"]);
    // Push the tag.
    run("git", ["push", "--follow-tags"]);
    console.log("release: done (local).");
    return;
  }

  ensureGhAuth();

  if (args.dryRun) {
    console.log("release: dry-run; would run:");
    console.log(`  npm version ${args.bump} -m "chore(release): %s"`);
    console.log("  git push --follow-tags");
    console.log("  (then GitHub Actions builds + publishes)");
    return;
  }

  // Make sure CI will not fight us: refuse if a tag for the next version already exists.
  // (npm version will fail later anyway, but the error is clearer here.)

  console.log("release: bumping version and tagging");
  run("npm", ["version", args.bump, "-m", `chore(release): %s`]);

  const bumped = readPkg();
  const tag = `v${bumped.version}`;
  console.log(`release: pushing branch ${branch} + tag ${tag}`);
  run("git", ["push", "--follow-tags"]);

  console.log(
    `release: tag pushed. GitHub Actions will build all platforms and publish ${bumped.name}@${bumped.version} to npm.`,
  );
  console.log(`release: follow progress at https://github.com/RenKoya1/claude-app-server/actions`);
}

main().catch((e) => {
  bail(e.message);
});
