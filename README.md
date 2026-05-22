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

## Building a custom agent

`thread/start` and `turn/start` are **open-ended**: any field that the
official `@anthropic-ai/claude-agent-sdk` `Options` type accepts is
forwarded verbatim into the underlying `query({ options })` call. No
protocol bump is needed to use new SDK options.

Common knobs:

| `params` key             | Effect                                                                  |
|--------------------------|-------------------------------------------------------------------------|
| `model`                  | Pick the model (`claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`, ...). |
| `systemPrompt`           | Replace the default Claude Code system prompt with your own.            |
| `cwd`                    | Working directory the agent operates from.                              |
| `additionalDirectories`  | Extra directories the agent may read/write beyond `cwd`.                |
| `permissionMode`         | `default` / `acceptEdits` / `bypassPermissions` / `plan` / `delegate` / `dontAsk`. |
| `allowedTools`           | Restrict the agent to a tool whitelist (e.g. `["Read", "Grep"]`).       |
| `disallowedTools`        | Tool blacklist (e.g. `["Bash"]` to forbid shell exec).                  |
| `maxTurns`               | Cap the autonomous tool-use loop length.                                |
| `env`                    | Env vars passed into the agent's child processes.                       |
| `mcpServers`             | Per-thread MCP server registration (DB connectors, internal tools, ...).|
| `agents`                 | Programmatic subagents reachable via the `Task` tool.                   |

Example — a SQL-only agent with a DB-backed MCP server:

```json
{
  "method": "thread/start",
  "id": 1,
  "params": {
    "model": "claude-sonnet-4-6",
    "systemPrompt": "You are a SQL expert. Use the `db` MCP to query.",
    "permissionMode": "plan",
    "allowedTools": ["Read", "Grep"],
    "maxTurns": 8,
    "mcpServers": {
      "db": {
        "type": "stdio",
        "command": "node",
        "args": ["mcp-postgres.js"],
        "env": { "DATABASE_URL": "postgres://localhost/app" }
      }
    },
    "agents": {
      "performance-reviewer": {
        "description": "Reviews SQL plans for performance issues.",
        "prompt": "You audit query plans. Use EXPLAIN ANALYZE.",
        "tools": ["Read"]
      }
    }
  }
}
```

Notes:

- The frontend does **not** type-check unknown keys; the SDK will reject
  malformed shapes and the server emits a `turn/completed` with
  `isError: true` and the SDK error in `errors[]`.
- Some SDK options (notably `hooks` and `abortController`) are
  JavaScript callbacks and cannot cross the JSON-RPC boundary; they are
  ignored.
- The full SDK `Options` reference:
  https://github.com/anthropics/claude-agent-sdk-typescript

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

#### Lifecycle

| Method        | Notes                                                          |
|---------------|----------------------------------------------------------------|
| `initialize`  | Handshake; must precede every other request on a connection.   |
| `initialized` | Notification acking the handshake.                             |

#### Threads

| Method                 | Notes                                                                  |
|------------------------|------------------------------------------------------------------------|
| `thread/start`         | Create a thread. Accepts `model`, `cwd`, `ephemeral`, `systemPrompt`.  |
| `thread/resume`        | Reopen a thread by id; supports `excludeTurns`.                        |
| `thread/fork`          | Branch a thread; `ephemeral` supported.                                |
| `thread/list`          | In-memory list, sorted newest first.                                   |
| `thread/loaded/list`   | Ids of currently loaded threads.                                       |
| `thread/read`          | Returns the stored thread; `includeTurns` hydrates history.            |
| `thread/archive`       | Move thread out of the active set; emits `thread/archived`.            |
| `thread/unarchive`     | Restore an archived thread; emits `thread/unarchived`.                 |
| `thread/unsubscribe`   | Stop receiving events for a thread; emits `thread/closed`.             |
| `thread/name/set`      | Rename a thread; emits `thread/name/updated`.                          |
| `thread/inject_items`  | Push raw SDK user messages into a session's input stream.              |
| `thread/compact/start` | Request manual context compaction.                                     |
| `thread/goal/set`      | Create/update the persisted goal; emits `thread/goal/updated`.         |
| `thread/goal/get`      | Read the current goal (`null` if none).                                |
| `thread/goal/clear`    | Remove the goal; emits `thread/goal/cleared` if state changed.         |

#### Turns

| Method           | Notes                                                                   |
|------------------|-------------------------------------------------------------------------|
| `turn/start`     | Stream a turn through the Claude Agent SDK. Tools/MCP/skills/hooks work.|
| `turn/interrupt` | Abort the in-flight turn via the sidecar's `AbortController`.           |
| `turn/steer`     | Append additional user input to the active turn.                        |

#### Models / config

| Method        | Notes                                                                |
|---------------|----------------------------------------------------------------------|
| `model/list`  | Reports Opus 4.7, Sonnet 4.6, Haiku 4.5.                             |
| `config/read` | Effective model, claude home, platform, current cwd.                 |

#### Skills / hooks / MCP

| Method                     | Notes                                                       |
|----------------------------|-------------------------------------------------------------|
| `skills/list`              | Discover `SKILL.md` files under `~/.claude/skills/`.        |
| `hooks/list`               | Placeholder until the SDK exposes a hook discovery API.     |
| `mcpServerStatus/list`     | Placeholder until the SDK exposes MCP status API.           |
| `mcpServer/tool/call`      | Currently returns `-32601`; route through `turn/start`.     |

#### Filesystem

| Method                | Notes                                                                |
|-----------------------|----------------------------------------------------------------------|
| `fs/readFile`         | Return base64-encoded bytes for an absolute file path.               |
| `fs/writeFile`        | Write base64-decoded bytes to an absolute path; parents auto-created.|
| `fs/createDirectory`  | `recursive` defaults to `true`.                                      |
| `fs/getMetadata`      | `isDirectory` / `isFile` / `isSymlink` / `createdAtMs` / `modifiedAtMs` / `sizeBytes`. |
| `fs/readDirectory`    | Sorted child entries with `isDirectory` / `isFile`.                  |
| `fs/remove`           | `recursive` / `force` default to `true`.                             |
| `fs/copy`             | Files copy directly; directories require `recursive: true`.          |
| `fs/watch`            | Cross-platform notify-backed watcher with `watchId`-keyed routing.   |
| `fs/unwatch`          | Cancel a registered watcher.                                         |

#### Command exec

| Method                    | Notes                                                                   |
|---------------------------|-------------------------------------------------------------------------|
| `command/exec`            | Spawn argv, capture stdout/stderr/exit. Optional streaming via `processId` + `streamStdoutStderr`. |
| `command/exec/write`      | Write base64 stdin to a running streaming process (or close stdin).     |
| `command/exec/terminate`  | Kill a running streaming process.                                       |

> `command/exec` is **unsandboxed**. Codex enforces a sandbox; we leave that to the host environment. Use only with trusted local UIs.

### Notifications emitted

- `thread/started`, `thread/archived`, `thread/unarchived`,
  `thread/name/updated`, `thread/goal/updated`, `thread/goal/cleared`,
  `thread/closed`.
- `thread/status/changed` on idle/active transitions.
- `turn/started`, `turn/completed` (with `tokenUsage`).
- `item/started`, `item/completed`, `item/agentMessage/delta`.
- `fs/changed` (per `watchId`).
- `command/exec/outputDelta` (when `streamStdoutStderr: true`).

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
| `process/spawn` / PTY exec  | Use `command/exec`. PTY mode + Windows sandbox not implemented. |
| `mcpServer*` rich queries   | Stub responses; SDK does not currently expose MCP status APIs.  |
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

## Contributing

We welcome PRs. Please read [`CONTRIBUTING.md`](CONTRIBUTING.md) before
opening one.

**Releases are restricted to maintainers.** Only repository maintainers
with push access to `RenKoya1/claude-app-server` (and the `NPM_TOKEN`
secret on this GitHub repo) can publish to npm. Contributors should:

- Fork the repo on GitHub.
- Push a feature branch to your fork.
- Open a PR against `RenKoya1/claude-app-server:main`.

A maintainer will review, merge, and cut the next release (see
[`RELEASING.md`](RELEASING.md) for the procedure they follow).
