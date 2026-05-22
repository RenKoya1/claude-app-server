# Releasing (maintainers only)

End users do not need this file. See [README.md](README.md) for install +
usage. Contributors should read [CONTRIBUTING.md](CONTRIBUTING.md).

## Access required

To cut a release you need **all** of the following. If you don't, stop:

- Push access to `RenKoya1/claude-app-server` `main` (to push commits + tags).
- `NPM_TOKEN` repository secret on the GitHub repo (set via
  `gh secret set NPM_TOKEN --repo RenKoya1/claude-app-server`).
- Local `gh` CLI authenticated to a GitHub account with the access above.

Contributors **cannot** publish to npm. The npm token is repo-scoped, only
the CI workflow can read it, and the workflow only runs on tag pushes to
`main` — both of which require maintainer push access.

End-to-end release is a single command:

```bash
npm run release patch          # 0.1.0 -> 0.1.1
npm run release minor          # 0.1.0 -> 0.2.0
npm run release major          # 0.1.0 -> 1.0.0
```

What this does locally: cleanliness check, version bump, git tag, push.

What GitHub Actions does once the tag lands
(`.github/workflows/release.yml`):

1. matrix-builds the Rust binary for `darwin-arm64`, `darwin-x64`,
   `linux-x64`, `linux-arm64`,
2. builds the TypeScript sidecar on a Linux runner,
3. assembles `dist/sidecar/` + `dist/bin/<triple>/` on a single publish
   job,
4. creates a GitHub Release with per-platform tarballs attached,
5. runs `npm publish --access public` against
   `@renkoya1/claude-app-server`.

## Prerequisites

One-time configuration on the GitHub repository:

- `NPM_TOKEN` repository secret — a granular token with publish rights
  to `@renkoya1/claude-app-server`
  (https://www.npmjs.com/settings/renkoya1/tokens). Add via:
  ```bash
  gh secret set NPM_TOKEN --repo RenKoya1/claude-app-server
  ```
- Default `GITHUB_TOKEN` (no setup needed) — used by `gh release create`.

Local prerequisites:

- `gh` CLI authenticated against `github.com/RenKoya1` (`gh auth login`).
- Clean git working tree on the release branch.

## Variants

Ship a one-platform build from your laptop without going through CI
(useful for dogfooding only):

```bash
npm run release patch -- --local
```

That bumps the version, builds the current platform, runs `npm publish`
directly, and then pushes the tag. The resulting package only works on
the host platform.

Preview the release flow without taking any action:

```bash
npm run release patch -- --dry-run
```

## Verifying after publish

```bash
npm view @renkoya1/claude-app-server version           # should match the new tag
npm view @renkoya1/claude-app-server dist.unpackedSize # sanity check size
```

Fresh install smoke test:

```bash
rm -rf /tmp/cas-check && mkdir /tmp/cas-check && cd /tmp/cas-check
npm init -y > /dev/null && npm install @renkoya1/claude-app-server
node_modules/.bin/claude-app-server < /dev/null
```

## Troubleshooting

- **CI stuck on a runner queue** — `macos-13` Intel runners are heavily
  contended. We cross-compile `darwin-x64` from `macos-14` to avoid the
  queue.
- **`npm publish` 401** — verify `NPM_TOKEN` is set, current, and has
  publish scope to `@renkoya1`.
- **`gh release create` 403** — the workflow needs `permissions: contents:
  write`; check `.github/workflows/release.yml`.
- **Package size regression** — run `npm pack --dry-run | grep "package
  size"` locally before tagging. Devs should be sub-15 MB tarball.

## Release checklist

1. `git status` clean, on `main`.
2. Tests pass locally (`cargo test --workspace`, sidecar `tsc --noEmit`).
3. `npm pack --dry-run` size looks reasonable (<20 MB tarball).
4. Bump: `npm run release patch|minor|major`.
5. Watch CI: `gh run watch --repo RenKoya1/claude-app-server`.
6. Verify on npm: `npm view @renkoya1/claude-app-server version`.
7. Verify install: see "Fresh install smoke test" above.
