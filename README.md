# claude-app-server

JSON-RPC server that exposes the official **Claude Agent SDK** to any client
over a wire protocol modeled on [`codex app-server`](https://github.com/openai/codex/tree/main/codex-rs/app-server).

```
┌─────────────────┐    JSON-RPC 2.0   ┌──────────────────────┐    @anthropic-ai/   ┌──────────────────┐
│ your client     │  (stdio JSONL)    │ claude-app-server    │  claude-agent-sdk   │  Anthropic API   │
│ (VS Code, web,  │ ◀───────────────▶ │ (Rust frontend +     │ ──────────────────▶ │  api.anthropic   │
│  desktop, CLI)  │                   │  TypeScript sidecar) │                     │  .com            │
└─────────────────┘                   └──────────────────────┘                     └──────────────────┘
```

Why two languages? Rust owns the protocol surface, transport, thread state
and multi-client fan-out. The TypeScript sidecar owns the agent loop
(tool use, MCP, hooks, skills, subagents, sessions) by hosting
`@anthropic-ai/claude-agent-sdk`, which is the official Claude SDK
implementation. The split keeps the protocol layer fast and statically
typed while letting the SDK update itself with Claude Code releases.

> **This is not** the official `claude` CLI or Claude Code product. It is a
> standalone server that lets you build your own client on top of the
> Claude agent. Authentication piggybacks on `claude login`.

## Install (users)

```bash
npx @renkoya1/claude-app-server          # one-off run
npm i -g @renkoya1/claude-app-server     # install globally
```

That's the whole install. `npm install` runs a `postinstall` step that
downloads the Rust binary for your platform from this package's GitHub
Release. Supported platforms: `darwin-arm64`, `darwin-x64`, `linux-x64`,
`linux-arm64`.

The package ships:
- `bin/launcher.mjs` — Node wrapper that exec's the Rust binary.
- `dist/sidecar/` — compiled TypeScript sidecar bundling
  `@anthropic-ai/claude-agent-sdk`.
- `dist/bin/<platform>/claude-app-server` — Rust binary for your platform.

## Authentication

This server uses `@anthropic-ai/claude-agent-sdk`, which is the same
engine that powers the `claude` CLI / Claude Code app. **If you have
already run `claude login`, you do not need any environment variables.**

Credential resolution order (same as the official `claude` CLI):

1. `ANTHROPIC_API_KEY` env var, if set, takes precedence.
2. OAuth tokens written by `claude login`:
   - macOS: Keychain entry `Claude Code-credentials`.
   - Linux / Windows: `~/.claude/.credentials.json`.
3. AWS Bedrock / Google Vertex credentials, if configured for the SDK.

So the two usual setups are:

```bash
# You already use Claude Code — nothing to do.
claude login                                  # one-time
npx @renkoya1/claude-app-server

# Or you want raw API key access (no Claude subscription):
ANTHROPIC_API_KEY=sk-ant-... npx @renkoya1/claude-app-server
```

## Build from source (contributors)

You only need this section if you cloned the repo to develop the server
itself. End users should use `npx` above.

```bash
git clone https://github.com/RenKoya1/claude-app-server.git
cd claude-app-server
npm install                    # installs launcher + sidecar runtime deps
npm run build                  # cargo build --release + tsc + stage dist/
node bin/launcher.mjs          # smoke test
```

## Run

`claude-app-server` speaks newline-delimited JSON-RPC 2.0 on stdin/stdout.
Drive it from your client:

```bash
{
  printf '%s\n' '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"demo","version":"0.1.0"}}}'
  printf '%s\n' '{"method":"initialized","params":{}}'
  printf '%s\n' '{"id":2,"method":"thread/start","params":{"model":"claude-haiku-4-5-20251001"}}'
  # remember thr_xxx from id:2 result
  printf '%s\n' '{"id":3,"method":"turn/start","params":{"threadId":"thr_xxx","input":[{"type":"text","text":"hello"}]}}'
} | npx claude-app-server
```

You will see `item/agentMessage/delta` notifications stream the response,
followed by a final `turn/completed` with token usage.

### Environment variables (all optional)

| Var                            | Meaning                                                                 |
|--------------------------------|-------------------------------------------------------------------------|
| `ANTHROPIC_MODEL`              | default model when `thread/start` omits one.                            |
| `ANTHROPIC_API_KEY`            | raw API key. Only needed if you don't use `claude login` for auth.      |
| `CLAUDE_HOME`                  | reported as `codexHome` in `initialize` (default `~/.claude`).          |
| `CLAUDE_APP_SERVER_SIDECAR`    | override the sidecar script path. Auto-set by the launcher.             |
| `CLAUDE_APP_SERVER_SKIP_DOWNLOAD=1` | skip the postinstall binary download.                              |
| `CLAUDE_APP_SERVER_RELEASE_URL` | override the binary download template used by postinstall.            |
| `RUST_LOG`                     | tracing filter, e.g. `info,claude_app_server=debug`.                    |
| `LOG_FORMAT=json`              | emit JSON tracing lines on stderr.                                      |

## Protocol overview

The wire protocol mirrors `codex app-server` so a client already written
against Codex can talk to this server with minimal changes:

- Newline-delimited JSON. `jsonrpc` field omitted.
- Required `initialize` handshake per connection. Subsequent requests
  before `initialized` are rejected with code `-32002`.
- Repeated `initialize` calls return `-32003 Already initialized`.
- When the ingress queue saturates, requests are rejected with code
  `-32001 Server overloaded; retry later.` — retry with backoff.

### Supported methods

| Method            | Notes                                                                  |
|-------------------|------------------------------------------------------------------------|
| `initialize`      | Handshake; must precede every other request on a connection.           |
| `initialized`     | Notification acking the handshake.                                     |
| `thread/start`    | Create a thread. Accepts `model`, `cwd`, `ephemeral`, `systemPrompt`.  |
| `thread/resume`   | Reopen a thread by id; supports `excludeTurns`.                        |
| `thread/fork`     | Branch a thread; `ephemeral` supported.                                |
| `thread/list`     | In-memory list, sorted newest first.                                   |
| `thread/read`     | Returns the stored thread; `includeTurns` controls history hydration.  |
| `thread/archive`  | Drops the thread from in-memory store and closes its sidecar session.  |
| `turn/start`      | Streams a turn through the Claude Agent SDK. Tools/MCP/skills/hooks work. |
| `turn/interrupt`  | Aborts the in-flight turn via the sidecar's AbortController.           |
| `model/list`      | Reports Opus 4.7, Sonnet 4.6, Haiku 4.5.                               |

### Notifications emitted

- `thread/started` after `thread/start`, `thread/resume`, `thread/fork`.
- `thread/status/changed` whenever a thread transitions between `idle` and `active`.
- `turn/started` immediately after `turn/start` accepts.
- `item/started` / `item/completed` for assistant messages, tool uses, tool
  results, reasoning.
- `item/agentMessage/delta` for each streamed text chunk.
- `turn/completed` with `tokenUsage`.

### Item kinds streamed

- `userMessage` — recorded on `turn/start`.
- `agentMessage` — the assistant's text reply.
- `toolUse` — the assistant invoked a tool (`Bash`, `Read`, `Edit`, MCP tool, …).
- `toolResult` — the result of a tool invocation; `isError` flags failures.
- `reasoning` — extended thinking output, when enabled.

## Gaps vs Codex app-server

These exist in `codex-rs/app-server` but are out of scope for now. A client
touching them gets `-32601 Method not supported`.

| Area                        | Status                                                          |
|-----------------------------|-----------------------------------------------------------------|
| websocket / unix-socket     | Only `--listen stdio://` is wired up.                           |
| Persistence                 | Threads + rollouts are RAM-only and lost on restart.            |
| `command/exec`, `process/spawn` | Not exposed — the Claude Agent SDK runs commands as tools instead. |
| `fs/*`                      | Not implemented; the SDK exposes `Read` / `Write` / `Edit` tools. |
| `mcpServer*` direct calls   | Not implemented; configure MCP servers via SDK options instead. |
| `marketplace/*`, `plugin/*` | No plugin/marketplace concept.                                  |
| `review/start`              | No automated reviewer pipeline.                                 |
| `thread/realtime/*`         | No realtime / WebRTC bridge.                                    |
| `feedback/upload`, `windowsSandbox/*`, `experimentalFeature/*`, ... | Not implemented. |
| `turn/steer`                | Not implemented; AbortController-based interrupt is.            |
| `thread/compact/start`, `thread/shellCommand`, `thread/inject_items` | Not implemented. |
| Schema export               | No `generate-ts` / `generate-json-schema` subcommands yet.      |

## Repository layout

```
claude/
├── package.json           # npm package manifest
├── bin/launcher.mjs       # `claude-app-server` entry point
├── scripts/
│   ├── build-local.sh     # cargo + tsc + stage dist/
│   └── postinstall.mjs    # download platform binary on `npm install`
├── dist/
│   ├── sidecar/           # compiled TS sidecar (shipped)
│   └── bin/<triple>/      # Rust binary (shipped)
├── crates/                # Rust source (not shipped; use cargo)
│   ├── claude-app-server/
│   ├── claude-app-server-protocol/
│   └── claude-app-server-transport/
├── sidecar/               # TS source (not shipped)
│   ├── package.json
│   ├── tsconfig.json
│   └── src/index.ts
└── README.md
```

## Building a custom client

The simplest client is just JSON-RPC over a process pipe:

```ts
import { spawn } from "node:child_process";

const server = spawn("npx", ["claude-app-server"], {
  stdio: ["pipe", "pipe", "inherit"],
});

let nextId = 1;
function send(method: string, params?: unknown) {
  const id = nextId++;
  server.stdin.write(JSON.stringify({ id, method, params }) + "\n");
  return id;
}

server.stdout.on("data", (chunk) => {
  for (const line of chunk.toString().split("\n")) {
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    if (msg.method === "item/agentMessage/delta") {
      process.stdout.write(msg.params.delta);
    } else if (msg.method === "turn/completed") {
      console.log("\n[done]", msg.params.tokenUsage);
    }
  }
});

send("initialize", { clientInfo: { name: "demo", version: "0.0.1" } });
server.stdin.write(JSON.stringify({ method: "initialized", params: {} }) + "\n");
const threadId = "<read from id:1 response>";
send("turn/start", { threadId, input: [{ type: "text", text: "hello" }] });
```

(Real clients should track request ids, route responses, and respect the
initialize handshake before sending other requests.)

## Caveats

- The Rust → TypeScript IPC is internal and may change between versions.
  Speak to the JSON-RPC surface, not the sidecar.
- `claude login` itself is not implemented here. Run the official `claude`
  CLI once to populate credentials.
- The bundled binary is downloaded on `npm install`. Sandboxed CI
  environments without outbound HTTP need `CLAUDE_APP_SERVER_RELEASE_URL`
  pointing at a mirror or `npm run build` from a source checkout.

## Publishing (maintainers)

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
5. runs `npm publish --access public` against `@renkoya1/claude-app-server`.

Prerequisites — these are one-time configuration on the GitHub
repository:

- `NPM_TOKEN` repository secret — a granular token with publish rights
  to `@renkoya1/claude-app-server` (https://www.npmjs.com/settings/renkoya1/tokens).
- Default `GITHUB_TOKEN` (no setup needed) — used by `gh release create`.

If you want to ship a one-platform build from your laptop without going
through CI (useful for dogfooding only):

```bash
npm run release patch -- --local
```

That bumps the version, builds the current platform, runs `npm publish`
directly, and then pushes the tag.

To preview the release flow without taking any action:

```bash
npm run release patch -- --dry-run
```
