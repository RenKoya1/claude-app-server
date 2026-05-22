/**
 * Claude app-server sidecar.
 *
 * Long-lived Node process spawned by the Rust frontend. Communicates over
 * stdin/stdout using newline-delimited JSON. The Rust side issues commands
 * (createSession, turn, interrupt, closeSession). This process drives the
 * official `@anthropic-ai/claude-agent-sdk` and streams agent events back.
 *
 * The contract is intentionally small and asymmetric: commands carry an `id`
 * and get a synchronous `ack`; agent activity is streamed as unsolicited
 * events tagged with `sessionId` + `turnId`.
 */

import { query, type Options, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import * as readline from "node:readline";

// --- IPC frames ---------------------------------------------------------

interface SessionOptions {
  cwd?: string;
  model?: string;
  systemPrompt?: string | null;
  permissionMode?:
    | "default"
    | "acceptEdits"
    | "bypassPermissions"
    | "plan"
    | "delegate"
    | "dontAsk";
  allowedTools?: string[];
  disallowedTools?: string[];
  maxTurns?: number;
  includePartialMessages?: boolean;
  resume?: string;
}

type InboundCommand =
  | {
      id: string;
      type: "createSession";
      sessionId: string;
      options?: SessionOptions;
    }
  | { id: string; type: "turn"; sessionId: string; turnId: string; input: TurnInput[] }
  | { id: string; type: "steerTurn"; sessionId: string; turnId: string; input: TurnInput[] }
  | {
      id: string;
      type: "injectItems";
      sessionId: string;
      items: unknown[];
    }
  | { id: string; type: "interrupt"; sessionId: string; turnId: string }
  | { id: string; type: "compact"; sessionId: string }
  | { id: string; type: "closeSession"; sessionId: string }
  | { id: string; type: "listSkills"; cwds?: string[] }
  | { id: string; type: "listHooks"; cwds?: string[] }
  | { id: string; type: "listMcpServers"; sessionId?: string }
  | {
      id: string;
      type: "callMcpTool";
      sessionId?: string;
      server: string;
      tool: string;
      arguments?: Record<string, unknown>;
    }
  | { id: string; type: "shutdown" };

type TurnInput =
  | { type: "text"; text: string }
  | { type: "image"; url: string }
  | { type: "localImage"; path: string };

type OutboundEvent =
  | { type: "ack"; id: string; ok: true }
  | { type: "ack"; id: string; ok: false; error: string }
  // Acks that carry a result payload for query-style commands.
  | {
      type: "result";
      id: string;
      ok: true;
      payload: unknown;
    }
  | { type: "ready" }
  | { type: "log"; level: "info" | "warn" | "error"; msg: string }
  | { type: "sessionReady"; sessionId: string }
  | { type: "sessionClosed"; sessionId: string; reason?: string }
  | { type: "turnStarted"; sessionId: string; turnId: string }
  | { type: "assistantDelta"; sessionId: string; turnId: string; itemId: string; delta: string }
  | { type: "assistantMessage"; sessionId: string; turnId: string; itemId: string; text: string }
  | {
      type: "toolUse";
      sessionId: string;
      turnId: string;
      toolUseId: string;
      name: string;
      input: unknown;
    }
  | {
      type: "toolResult";
      sessionId: string;
      turnId: string;
      toolUseId: string;
      content: unknown;
      isError: boolean;
    }
  | { type: "reasoning"; sessionId: string; turnId: string; itemId: string; text: string }
  | {
      type: "turnCompleted";
      sessionId: string;
      turnId: string;
      isError: boolean;
      result?: string;
      errors?: string[];
      usage?: unknown;
      totalCostUsd?: number;
      durationMs?: number;
      numTurns?: number;
    };

// --- Output helpers -----------------------------------------------------

function emit(event: OutboundEvent): void {
  process.stdout.write(JSON.stringify(event) + "\n");
}

function log(level: "info" | "warn" | "error", msg: string): void {
  emit({ type: "log", level, msg });
}

// --- Session state ------------------------------------------------------

class SessionInputBuffer implements AsyncIterable<SDKUserMessage> {
  private queue: SDKUserMessage[] = [];
  private resolvers: ((value: IteratorResult<SDKUserMessage>) => void)[] = [];
  private closed = false;

  push(message: SDKUserMessage): void {
    if (this.closed) return;
    const resolver = this.resolvers.shift();
    if (resolver) {
      resolver({ value: message, done: false });
    } else {
      this.queue.push(message);
    }
  }

  close(): void {
    this.closed = true;
    while (this.resolvers.length > 0) {
      const resolver = this.resolvers.shift()!;
      resolver({ value: undefined as unknown as SDKUserMessage, done: true });
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    return {
      next: () => {
        const queued = this.queue.shift();
        if (queued) {
          return Promise.resolve({ value: queued, done: false });
        }
        if (this.closed) {
          return Promise.resolve({ value: undefined as unknown as SDKUserMessage, done: true });
        }
        return new Promise<IteratorResult<SDKUserMessage>>((resolve) => {
          this.resolvers.push(resolve);
        });
      },
      return: () => {
        this.close();
        return Promise.resolve({ value: undefined as unknown as SDKUserMessage, done: true });
      },
    };
  }
}

interface Session {
  sessionId: string;
  input: SessionInputBuffer;
  abort: AbortController;
  activeTurnId: string | null;
  options: SessionOptions;
  loopPromise: Promise<void>;
  sdkSessionId: string | null;
}

const sessions = new Map<string, Session>();

// --- Command handlers ---------------------------------------------------

function buildOptions(spec: SessionOptions, abort: AbortController): Options {
  const opts: Options = {
    abortController: abort,
    includePartialMessages: spec.includePartialMessages ?? true,
    cwd: spec.cwd,
    model: spec.model,
    permissionMode: spec.permissionMode ?? "bypassPermissions",
    allowedTools: spec.allowedTools,
    disallowedTools: spec.disallowedTools,
    maxTurns: spec.maxTurns,
    resume: spec.resume,
  };
  if (spec.systemPrompt === null) {
    opts.systemPrompt = { type: "preset", preset: "claude_code" } as unknown as Options["systemPrompt"];
  } else if (typeof spec.systemPrompt === "string") {
    opts.systemPrompt = spec.systemPrompt;
  }
  // Strip undefined entries so SDK defaults apply.
  for (const k of Object.keys(opts) as (keyof Options)[]) {
    if (opts[k] === undefined) delete opts[k];
  }
  return opts;
}

function inputToBlocks(input: TurnInput[]): SDKUserMessage["message"]["content"] {
  return input.map((chunk): { type: "text"; text: string } | { type: "image"; source: { type: "url"; url: string } | { type: "base64"; media_type: string; data: string } } => {
    switch (chunk.type) {
      case "text":
        return { type: "text", text: chunk.text };
      case "image":
        return { type: "image", source: { type: "url", url: chunk.url } };
      case "localImage":
        // Inline as base64 to keep parity with the Rust client.
        // Defer fs import for cold-start time on text-only sessions.
        // eslint-disable-next-line @typescript-eslint/no-require-imports
        const fs = require("node:fs") as typeof import("node:fs");
        const path = require("node:path") as typeof import("node:path");
        const bytes = fs.readFileSync(chunk.path);
        const ext = path.extname(chunk.path).slice(1).toLowerCase();
        const mediaType = ext === "jpg" ? "image/jpeg" : ext === "png" ? "image/png" : ext === "gif" ? "image/gif" : ext === "webp" ? "image/webp" : "application/octet-stream";
        return {
          type: "image",
          source: { type: "base64", media_type: mediaType, data: bytes.toString("base64") },
        };
    }
  }) as unknown as SDKUserMessage["message"]["content"];
}

async function runSessionLoop(session: Session): Promise<void> {
  const options = buildOptions(session.options, session.abort);
  let assistantItemId: string | null = null;
  try {
    const stream = query({ prompt: session.input, options });
    for await (const message of stream) {
      switch (message.type) {
        case "system": {
          if ("subtype" in message && message.subtype === "init") {
            session.sdkSessionId = message.session_id ?? null;
          }
          break;
        }
        case "stream_event": {
          // partial assistant streaming. We only forward text_delta.
          const ev = message.event as unknown as {
            type?: string;
            delta?: { type?: string; text?: string };
            content_block?: { type?: string };
            index?: number;
          };
          if (ev?.type === "content_block_start" && ev.content_block?.type === "text") {
            assistantItemId = `msg_${cryptoRandom()}`;
          }
          if (ev?.type === "content_block_delta" && ev.delta?.type === "text_delta" && ev.delta.text) {
            if (!assistantItemId) assistantItemId = `msg_${cryptoRandom()}`;
            emit({
              type: "assistantDelta",
              sessionId: session.sessionId,
              turnId: session.activeTurnId ?? "",
              itemId: assistantItemId,
              delta: ev.delta.text,
            });
          }
          break;
        }
        case "assistant": {
          const blocks = (message.message?.content ?? []) as Array<{
            type: string;
            text?: string;
            id?: string;
            name?: string;
            input?: unknown;
            thinking?: string;
          }>;
          for (const block of blocks) {
            if (block.type === "text" && block.text) {
              emit({
                type: "assistantMessage",
                sessionId: session.sessionId,
                turnId: session.activeTurnId ?? "",
                itemId: assistantItemId ?? `msg_${cryptoRandom()}`,
                text: block.text,
              });
              assistantItemId = null;
            } else if (block.type === "tool_use") {
              emit({
                type: "toolUse",
                sessionId: session.sessionId,
                turnId: session.activeTurnId ?? "",
                toolUseId: block.id ?? `tu_${cryptoRandom()}`,
                name: block.name ?? "unknown",
                input: block.input,
              });
            } else if (block.type === "thinking" && block.thinking) {
              emit({
                type: "reasoning",
                sessionId: session.sessionId,
                turnId: session.activeTurnId ?? "",
                itemId: `rsn_${cryptoRandom()}`,
                text: block.thinking,
              });
            }
          }
          break;
        }
        case "user": {
          const blocks = (message.message?.content ?? []) as Array<{
            type: string;
            tool_use_id?: string;
            content?: unknown;
            is_error?: boolean;
          }>;
          for (const block of blocks) {
            if (block.type === "tool_result" && block.tool_use_id) {
              emit({
                type: "toolResult",
                sessionId: session.sessionId,
                turnId: session.activeTurnId ?? "",
                toolUseId: block.tool_use_id,
                content: block.content,
                isError: Boolean(block.is_error),
              });
            }
          }
          break;
        }
        case "result": {
          const turnId = session.activeTurnId ?? "";
          const completed: OutboundEvent = {
            type: "turnCompleted",
            sessionId: session.sessionId,
            turnId,
            isError: message.is_error,
            usage: message.usage,
            totalCostUsd: message.total_cost_usd,
            durationMs: message.duration_ms,
            numTurns: message.num_turns,
          };
          if ("result" in message && typeof message.result === "string") {
            completed.result = message.result;
          }
          if ("errors" in message && Array.isArray(message.errors)) {
            completed.errors = message.errors;
          }
          emit(completed);
          session.activeTurnId = null;
          assistantItemId = null;
          break;
        }
      }
    }
  } catch (e) {
    const err = e instanceof Error ? e.message : String(e);
    log("error", `session ${session.sessionId} loop crashed: ${err}`);
    if (session.activeTurnId) {
      emit({
        type: "turnCompleted",
        sessionId: session.sessionId,
        turnId: session.activeTurnId,
        isError: true,
        errors: [err],
      });
    }
  } finally {
    emit({ type: "sessionClosed", sessionId: session.sessionId });
    sessions.delete(session.sessionId);
  }
}

function cryptoRandom(): string {
  // Avoid pulling in uuid as a dependency; collisions are not a concern here.
  return Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
}

function handleCommand(cmd: InboundCommand): void {
  switch (cmd.type) {
    case "createSession": {
      if (sessions.has(cmd.sessionId)) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "session already exists" });
        return;
      }
      const input = new SessionInputBuffer();
      const abort = new AbortController();
      const session: Session = {
        sessionId: cmd.sessionId,
        input,
        abort,
        activeTurnId: null,
        options: cmd.options ?? {},
        loopPromise: Promise.resolve(),
        sdkSessionId: null,
      };
      session.loopPromise = runSessionLoop(session);
      sessions.set(cmd.sessionId, session);
      emit({ type: "ack", id: cmd.id, ok: true });
      emit({ type: "sessionReady", sessionId: cmd.sessionId });
      return;
    }
    case "turn": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      if (session.activeTurnId) {
        emit({
          type: "ack",
          id: cmd.id,
          ok: false,
          error: `session has active turn ${session.activeTurnId}`,
        });
        return;
      }
      session.activeTurnId = cmd.turnId;
      const userMessage: SDKUserMessage = {
        type: "user",
        message: {
          role: "user",
          content: inputToBlocks(cmd.input),
        },
        parent_tool_use_id: null,
        session_id: session.sdkSessionId ?? session.sessionId,
      };
      session.input.push(userMessage);
      emit({ type: "ack", id: cmd.id, ok: true });
      emit({ type: "turnStarted", sessionId: cmd.sessionId, turnId: cmd.turnId });
      return;
    }
    case "steerTurn": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      if (!session.activeTurnId || session.activeTurnId !== cmd.turnId) {
        emit({
          type: "ack",
          id: cmd.id,
          ok: false,
          error: `no matching active turn (have ${session.activeTurnId ?? "none"})`,
        });
        return;
      }
      const steerMessage: SDKUserMessage = {
        type: "user",
        message: { role: "user", content: inputToBlocks(cmd.input) },
        parent_tool_use_id: null,
        session_id: session.sdkSessionId ?? session.sessionId,
      };
      session.input.push(steerMessage);
      emit({ type: "ack", id: cmd.id, ok: true });
      return;
    }
    case "injectItems": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      // The SDK does not currently expose a "inject raw items" API on
      // streaming sessions; we ack so clients have parity surface, but the
      // items only land if they parse as a valid SDK user message.
      for (const raw of cmd.items) {
        if (raw && typeof raw === "object") {
          session.input.push(raw as SDKUserMessage);
        }
      }
      emit({ type: "ack", id: cmd.id, ok: true });
      return;
    }
    case "interrupt": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      session.abort.abort();
      // Replace the aborted controller so the next `turn` can run without
      // having to recreate the whole session.
      session.abort = new AbortController();
      emit({ type: "ack", id: cmd.id, ok: true });
      return;
    }
    case "compact": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      // SDK exposes context compaction via /compact slash command in the
      // chat history. We approximate by pushing a user message that the
      // SDK will interpret as a compaction trigger.
      const trigger: SDKUserMessage = {
        type: "user",
        message: {
          role: "user",
          content: [{ type: "text", text: "/compact" }],
        },
        parent_tool_use_id: null,
        session_id: session.sdkSessionId ?? session.sessionId,
      };
      session.input.push(trigger);
      emit({ type: "ack", id: cmd.id, ok: true });
      return;
    }
    case "closeSession": {
      const session = sessions.get(cmd.sessionId);
      if (!session) {
        emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
        return;
      }
      session.input.close();
      session.abort.abort();
      emit({ type: "ack", id: cmd.id, ok: true });
      return;
    }
    case "listSkills": {
      // Best-effort enumeration: scan ~/.claude/skills/ and per-cwd
      // .claude/skills/ for SKILL.md files. The SDK loads skills at turn
      // time, so this only exposes their existence on disk.
      const fs = require("node:fs") as typeof import("node:fs");
      const path = require("node:path") as typeof import("node:path");
      const cwds = cmd.cwds ?? [];
      const homes = [process.env.CLAUDE_HOME ?? path.join(process.env.HOME ?? "", ".claude")];
      const data: Array<{ name: string; path: string; description?: string; cwd?: string }> = [];
      for (const root of [...homes, ...cwds.map((c) => path.join(c, ".claude"))]) {
        const skillsDir = path.join(root, "skills");
        if (!fs.existsSync(skillsDir)) continue;
        let entries: string[];
        try { entries = fs.readdirSync(skillsDir); } catch { continue; }
        for (const entry of entries) {
          const skillPath = path.join(skillsDir, entry, "SKILL.md");
          if (!fs.existsSync(skillPath)) continue;
          let description: string | undefined;
          try {
            const text = fs.readFileSync(skillPath, "utf8");
            const match = /^description:\s*(.+)$/m.exec(text);
            if (match) description = match[1].trim();
          } catch {}
          data.push({ name: entry, path: skillPath, description });
        }
      }
      emit({ type: "result", id: cmd.id, ok: true, payload: { data } });
      return;
    }
    case "listHooks": {
      // SDK uses programmatic hooks via Options.hooks at session creation;
      // there is no global discovery API. We surface an empty list rather
      // than fabricate one, matching codex contract for hooks/list.
      emit({ type: "result", id: cmd.id, ok: true, payload: { data: [] } });
      return;
    }
    case "listMcpServers": {
      // The SDK does not expose MCP server status via a stable API surface
      // in the current version. We return an empty list as a placeholder so
      // clients can call the endpoint without erroring; richer support
      // lands when the SDK exposes McpServerStatus.
      emit({ type: "result", id: cmd.id, ok: true, payload: { data: [] } });
      return;
    }
    case "callMcpTool": {
      // Same reason as above: MCP tool invocation goes through the agent
      // loop normally. We do not surface a separate call API yet.
      emit({
        type: "ack",
        id: cmd.id,
        ok: false,
        error: "mcpServer/tool/call: not supported yet (route tool calls through turn/start)",
      });
      return;
    }
    case "shutdown": {
      emit({ type: "ack", id: cmd.id, ok: true });
      for (const session of sessions.values()) {
        session.abort.abort();
        session.input.close();
      }
      // Give the loops a tick to finish, then exit.
      setTimeout(() => process.exit(0), 50);
      return;
    }
  }
}

// --- Entry point --------------------------------------------------------

function main(): void {
  // Refuse to emit anything on stdin error other than logs on stderr.
  process.stdin.on("error", (err) => {
    process.stderr.write(`stdin error: ${err}\n`);
  });

  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  rl.on("line", (line) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    let parsed: InboundCommand;
    try {
      parsed = JSON.parse(trimmed) as InboundCommand;
    } catch (e) {
      log("warn", `invalid command line: ${(e as Error).message}`);
      return;
    }
    try {
      handleCommand(parsed);
    } catch (e) {
      const err = e instanceof Error ? e.message : String(e);
      log("error", `command failed: ${err}`);
      if ("id" in parsed && typeof parsed.id === "string") {
        emit({ type: "ack", id: parsed.id, ok: false, error: err });
      }
    }
  });

  rl.on("close", () => {
    for (const session of sessions.values()) {
      session.abort.abort();
      session.input.close();
    }
    setTimeout(() => process.exit(0), 50);
  });

  emit({ type: "ready" });
}

main();
