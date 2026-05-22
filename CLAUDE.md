# CLAUDE.md — agent rules for this repository

This file controls how AI coding agents (Claude Code, etc.) work in this
repo. Human contributors should read [`RELEASING.md`](RELEASING.md) for
the procedural reference; this file is the **must-follow rules** for any
release-touching action.

---

## Hard rule: never publish without first testing

**Do not run `npm run release` (any variant) until ALL of these have
passed against the current working tree:**

1. **Workspace build** is clean:
   ```bash
   cargo check --workspace
   cargo build --release -p claude-app-server
   ```
2. **Sidecar type-check + compile** passes:
   ```bash
   (cd sidecar && npx tsc -p tsconfig.json)
   ```
3. **Local artifacts staged** via the canonical pipeline:
   ```bash
   ./scripts/build-local.sh
   ```
4. **Smoke test** against the freshly-built launcher succeeds (see
   *Required smoke test* below).
5. **`npm pack --dry-run`** size is below the budget:
   ```bash
   npm pack --dry-run 2>&1 | grep -E "package size|unpacked size"
   ```
   Budget: **tarball ≤ 15 MB**, **unpacked ≤ 85 MB**. If exceeded, halt
   and investigate — likely a regression in `npm prune --omit=dev` or
   ripgrep prune.
6. **Git working tree is clean** and on `main`. No
   `--no-verify`, no force-push to main, no skipping the release
   precheck in `scripts/release.mjs`.

If any of (1)–(6) fail or you cannot evaluate them, **stop and report
the failure** instead of attempting to publish.

---

## Required smoke test

Run before every release. The block below is the canonical input. If
exit code is non-zero, or any response carries `error`, abort.

```bash
cat <<'EOF' > /tmp/cas_smoke.jsonl
{"id":1,"method":"initialize","params":{"clientInfo":{"name":"smoke","version":"0.0.0"}}}
{"method":"initialized","params":{}}
{"id":2,"method":"config/read"}
{"id":3,"method":"fs/createDirectory","params":{"path":"/tmp/cas_smoke"}}
{"id":4,"method":"fs/writeFile","params":{"path":"/tmp/cas_smoke/hello.txt","dataBase64":"aGVsbG8="}}
{"id":5,"method":"fs/readFile","params":{"path":"/tmp/cas_smoke/hello.txt"}}
{"id":6,"method":"command/exec","params":{"command":["echo","ok"]}}
{"id":7,"method":"thread/start","params":{"model":"claude-haiku-4-5-20251001"}}
{"id":8,"method":"thread/loaded/list"}
{"id":9,"method":"fs/remove","params":{"path":"/tmp/cas_smoke"}}
EOF

node bin/launcher.mjs < /tmp/cas_smoke.jsonl > /tmp/cas_smoke_out.jsonl 2>/tmp/cas_smoke.err
echo "exit: $?"
grep -c '"error"' /tmp/cas_smoke_out.jsonl   # MUST be 0
cat /tmp/cas_smoke_out.jsonl                  # eyeball results
```

Expected: exit 0, zero `"error"` lines, every `"id":N` carries a `result`
object, plus a `thread/started` notification for id 7.

---

## Required post-publish verification

After CI succeeds and npm shows the new version, **always** verify a
fresh install on a real machine:

```bash
NEW_VERSION=$(node -p "require('./package.json').version")
TMPDIR=/tmp/cas-verify-${NEW_VERSION}
rm -rf "$TMPDIR" && mkdir "$TMPDIR" && cd "$TMPDIR"
npm init -y > /dev/null
npm install "@renkoya1/claude-app-server@${NEW_VERSION}"

# Run the same smoke test against the installed launcher.
node_modules/.bin/claude-app-server < /tmp/cas_smoke.jsonl > /tmp/cas_verify_out.jsonl
grep -c '"error"' /tmp/cas_verify_out.jsonl   # MUST be 0

# Size budget on user disk.
du -sh node_modules/@renkoya1/claude-app-server/   # < 60 MB
```

If verification fails: **immediately publish a patch** that fixes the
issue. Do not leave a broken version as `latest` on npm.

---

## Release flow (the only allowed sequence)

1. Make code change.
2. Run the precheck list above (sections "Hard rule" and
   "Required smoke test").
3. Commit on `main`.
4. `npm run release patch|minor|major`.
5. `gh run watch --repo RenKoya1/claude-app-server` until conclusion is
   `success`. If failure, fix and start over.
6. Run "Required post-publish verification" above.
7. Done.

No shortcuts. No `--local` publish for shippable versions (single-platform
binary makes the package broken for everyone else). No re-tag of an
existing version (npm forbids it, and even if it did not, it would break
caches everywhere).

---

## Version bump policy

- **patch** — bug fix, doc tweak, sidecar prune, CI workflow change.
- **minor** — new method, new event, new optional param.
- **major** — wire protocol breakage (renamed method, removed event,
  changed required param shape).

Pre-1.0 we still try to honor semver. After 1.0 it is binding.

---

## Things that always require a fresh release

- Touching `bin/launcher.mjs` (changes the entry point users execute).
- Touching `scripts/postinstall.mjs` (changes install-time behavior).
- Touching `package.json` `files` / `bin` / `engines` / `os` / `cpu`.
- Touching `dist/sidecar/` shape (the IPC contract with the Rust side).
- Bumping the Rust binary (any change under `crates/`).
- Bumping the sidecar (any change under `sidecar/src/`).

`README.md` / `CLAUDE.md` / `RELEASING.md` doc-only edits are allowed
without a release if they do not change package metadata. Push them to
`main`; npmjs.com will pick the README up from the next release.

---

## Anti-patterns (do NOT do these)

- ❌ `npm publish` directly (bypasses the `release.mjs` precheck).
- ❌ `npm run release ... -- --local` for a public version.
- ❌ Committing `target/`, `node_modules/`, `dist/`, or `.env`.
- ❌ Hardcoding any secret in source. Tokens live in:
  - **CI**: `gh secret set NPM_TOKEN`.
  - **Local**: `~/.npmrc` (via `npm login`).
- ❌ `git push --force` to `main`.
- ❌ `git rebase -i` on commits that are already in `origin/main`.
- ❌ Re-tagging an already-published version.
- ❌ Skipping the smoke test "because it's a small change". Small changes
  are the most likely to silently break the IPC.

---

## Repository invariants

- The sidecar (`sidecar/src/index.ts`) is the **only** code allowed to
  talk to `@anthropic-ai/claude-agent-sdk`. The Rust frontend must not
  import the SDK directly.
- The Rust ↔ TS IPC over stdio is **internal** and may change. Speak
  to the JSON-RPC surface from clients, not the sidecar protocol.
- `package.json` `files` whitelist must include only:
  `bin`, `dist/sidecar`, `scripts/postinstall.mjs`, `README.md`,
  `LICENSE`. Rust binaries come from GH Releases via postinstall — they
  must **not** be in `files`.
- README on npm comes from the README.md committed at the published
  tag. Edits to README without a release will not show on npm.

---

## How to read this file

When you are an AI agent operating in this repo: read this file in full
before any action whose blast radius reaches outside the working tree
(`git push`, `npm publish`, `gh release create`, `gh secret set`). If a
rule here conflicts with an inline user instruction, **prefer this file
unless the user explicitly references the conflicting rule and asks to
override it**.
