# Contributing

Thanks for taking the time to look at this project. PRs welcome.

## Who can do what

| Action                                                        | Who          |
|---------------------------------------------------------------|--------------|
| Open issues, propose features, ask questions                  | Anyone       |
| Fork the repo + open PRs                                      | Anyone       |
| Merge PRs into `main`                                         | Maintainers  |
| Push tags / cut releases / publish to npm                     | Maintainers  |
| Edit GitHub Secrets (`NPM_TOKEN`, etc.)                       | Maintainers  |

Contributors **cannot publish** versions of this package. The release
flow requires:

1. Push access to `RenKoya1/claude-app-server` (to push a release tag).
2. The `NPM_TOKEN` repository secret (only set on this GitHub repo).

Both are restricted to repository maintainers. If you contributed a
change you want released, ping a maintainer in the PR.

## Setting up a dev environment

```bash
git clone https://github.com/<your-fork>/claude-app-server.git
cd claude-app-server
npm install                    # installs sidecar runtime + launcher
npm run build                  # cargo + tsc + stage dist/
node bin/launcher.mjs          # smoke test the wrapper locally
```

Requirements:

- Node.js ≥ 18
- Rust toolchain (`rustup`)
- macOS or Linux (Windows untested)

## Before opening a PR

Run the same gates the release script runs:

```bash
# Type checks
cargo check --workspace
(cd sidecar && npx tsc -p tsconfig.json --noEmit)

# Build local artifacts
./scripts/build-local.sh

# Smoke test — the exact input from CLAUDE.md
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
node bin/launcher.mjs < /tmp/cas_smoke.jsonl > /tmp/cas_smoke_out.jsonl
grep -c '"error"' /tmp/cas_smoke_out.jsonl   # MUST be 0
```

All checks must pass before requesting review. The release script
re-runs the smoke test before publishing — if anything is broken, the
release is aborted.

## PR style

- Keep PRs focused. One feature / fix per PR.
- Match existing code style (Rust: `cargo fmt`; TS: project defaults).
- Update README.md / CLAUDE.md when behavior changes.
- For new JSON-RPC endpoints, also update the schema export in
  `crates/claude-app-server/src/schema_export.rs`.

## Anti-patterns (will be rejected on review)

- Skipping the smoke test or release gates.
- Committing `target/`, `node_modules/`, `dist/`, or `.env`.
- Hardcoding secrets in source.
- Changing wire protocol shape without bumping the minor or major
  version in the same PR.
- Adding `process.env.*` reads outside the documented variable list.
