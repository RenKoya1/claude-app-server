/**
 * Claude app-server sidecar.
 *
 * Long-lived Node process spawned by the Rust frontend. Communicates over
 * stdin/stdout using newline-delimited JSON. The Rust side issues commands
 * (createSession, turn, interrupt, closeSession, query-control commands).
 * This process drives `@anthropic-ai/claude-agent-sdk` and streams agent
 * activity back as events.
 *
 * Design rules:
 *  - SessionOptions is an opaque JSON blob. Every key the SDK Options type
 *    accepts works without sidecar code changes. Bridge flags
 *    (`bridgeCanUseTool`, `bridgeHooks`) opt-in to event-based callbacks.
 *  - One persistent Query handle per session. Control commands
 *    (setModel, setPermissionMode, interrupt, rewindFiles, ...) operate
 *    on the live handle instead of recreating the loop.
 *  - canUseTool + hooks are bridged through `permissionRequest` /
 *    `hookStarted` / `hookCompleted` events plus a `permissionResponse`
 *    inbound command to fulfill blocking promises.
 */
import { query, HOOK_EVENTS, } from "@anthropic-ai/claude-agent-sdk";
import * as readline from "node:readline";
// --- Output helpers -----------------------------------------------------
function emit(event) {
    process.stdout.write(JSON.stringify(event) + "\n");
}
function log(level, msg) {
    emit({ type: "log", level, msg });
}
// --- Session state ------------------------------------------------------
class SessionInputBuffer {
    queue = [];
    resolvers = [];
    closed = false;
    push(message) {
        if (this.closed)
            return;
        const resolver = this.resolvers.shift();
        if (resolver) {
            resolver({ value: message, done: false });
        }
        else {
            this.queue.push(message);
        }
    }
    close() {
        this.closed = true;
        while (this.resolvers.length > 0) {
            const resolver = this.resolvers.shift();
            resolver({ value: undefined, done: true });
        }
    }
    [Symbol.asyncIterator]() {
        return {
            next: () => {
                const queued = this.queue.shift();
                if (queued) {
                    return Promise.resolve({ value: queued, done: false });
                }
                if (this.closed) {
                    return Promise.resolve({ value: undefined, done: true });
                }
                return new Promise((resolve) => {
                    this.resolvers.push(resolve);
                });
            },
            return: () => {
                this.close();
                return Promise.resolve({ value: undefined, done: true });
            },
        };
    }
}
const sessions = new Map();
function newReqId(prefix) {
    return `${prefix}_${Math.random().toString(36).slice(2, 10)}${Date.now().toString(36)}`;
}
// --- Options builder ---------------------------------------------------
const BRIDGE_KEYS = new Set(["bridgeCanUseTool", "bridgeHooks", "bridgeHookResponses"]);
const HOOK_EVENT_NAMES = HOOK_EVENTS;
function buildOptions(session) {
    const raw = session.options ?? {};
    const opts = {};
    // Pass through any non-bridge key the SDK Options type accepts. We do
    // not enumerate the keys here on purpose — the SDK validates them and
    // any future field works without sidecar changes.
    for (const [k, v] of Object.entries(raw)) {
        if (BRIDGE_KEYS.has(k))
            continue;
        if (v === undefined)
            continue;
        opts[k] = v;
    }
    // Always inject the abort controller from the session.
    opts.abortController = session.abort;
    // Default to partial-message streaming so the Rust frontend can
    // forward assistantDelta + reasoningDelta without extra plumbing.
    if (opts.includePartialMessages === undefined) {
        opts.includePartialMessages = true;
    }
    // Safety: bypassPermissions requires the SDK's explicit opt-in.
    if (opts.permissionMode === "bypassPermissions" && opts.allowDangerouslySkipPermissions === undefined) {
        opts.allowDangerouslySkipPermissions = true;
    }
    // systemPrompt sentinel: explicit `null` means "use claude_code preset".
    if (opts.systemPrompt === null) {
        opts.systemPrompt = { type: "preset", preset: "claude_code" };
    }
    // mcpServers: clients pass plain JSON objects. The SDK accepts those
    // directly as McpServerConfig (process-transport variants). Anything
    // requiring an in-process McpServer instance must be configured via a
    // future `bridgeMcpServers` flag — not in scope here.
    // (No-op: opts.mcpServers already passed through.)
    // Hooks bridge ------------------------------------------------------
    const hookSel = raw.bridgeHooks;
    const wantHookResp = Boolean(raw.bridgeHookResponses);
    if (hookSel) {
        const events = hookSel === "all" ? HOOK_EVENT_NAMES : (Array.isArray(hookSel) ? hookSel : []);
        const hooks = {};
        for (const event of events) {
            hooks[event] = [
                {
                    hooks: [makeHookCallback(session, event, wantHookResp)],
                },
            ];
        }
        opts.hooks = hooks;
    }
    // canUseTool bridge -------------------------------------------------
    if (raw.bridgeCanUseTool) {
        opts.canUseTool = makeCanUseTool(session);
    }
    return opts;
}
function makeHookCallback(session, event, wantResponse) {
    return async (input, toolUseId) => {
        const requestId = newReqId("hook");
        if (wantResponse) {
            return await new Promise((resolve, reject) => {
                session.pendingHooks.set(requestId, { resolve, reject });
                emit({
                    type: "hookEvent",
                    sessionId: session.sessionId,
                    requestId,
                    event,
                    toolUseId,
                    payload: input,
                    expectResponse: true,
                });
            });
        }
        emit({
            type: "hookEvent",
            sessionId: session.sessionId,
            requestId,
            event,
            toolUseId,
            payload: input,
            expectResponse: false,
        });
        return { continue: true };
    };
}
function makeCanUseTool(session) {
    return async (toolName, input, options) => {
        const requestId = newReqId("perm");
        return await new Promise((resolve, reject) => {
            session.pendingPermissions.set(requestId, {
                resolve,
                reject,
                toolUseID: options.toolUseID,
            });
            emit({
                type: "permissionRequest",
                sessionId: session.sessionId,
                requestId,
                toolName,
                toolUseId: options.toolUseID,
                input,
                suggestions: options.suggestions,
                blockedPath: options.blockedPath,
                decisionReason: options.decisionReason,
                agentId: options.agentID,
            });
            // Hook the abort signal so a session shutdown unblocks the SDK.
            options.signal.addEventListener("abort", () => {
                if (session.pendingPermissions.delete(requestId)) {
                    reject(new Error("aborted"));
                }
            }, { once: true });
        });
    };
}
function inputToBlocks(input) {
    return input.map((chunk) => {
        switch (chunk.type) {
            case "text":
                return { type: "text", text: chunk.text };
            case "image":
                return { type: "image", source: { type: "url", url: chunk.url } };
            case "localImage": {
                // eslint-disable-next-line @typescript-eslint/no-require-imports
                const fs = require("node:fs");
                const path = require("node:path");
                const bytes = fs.readFileSync(chunk.path);
                const ext = path.extname(chunk.path).slice(1).toLowerCase();
                const mediaType = ext === "jpg" || ext === "jpeg" ? "image/jpeg"
                    : ext === "png" ? "image/png"
                        : ext === "gif" ? "image/gif"
                            : ext === "webp" ? "image/webp"
                                : "application/octet-stream";
                return {
                    type: "image",
                    source: { type: "base64", media_type: mediaType, data: bytes.toString("base64") },
                };
            }
        }
    });
}
async function runSessionLoop(session) {
    const options = buildOptions(session);
    let assistantItemId = null;
    let reasoningItemId = null;
    try {
        const stream = query({ prompt: session.input, options });
        session.query = stream;
        for await (const message of stream) {
            switch (message.type) {
                case "system": {
                    if ("subtype" in message && message.subtype === "init") {
                        session.sdkSessionId = message.session_id ?? null;
                        emit({
                            type: "sessionInit",
                            sessionId: session.sessionId,
                            sdkSessionId: message.session_id,
                            model: message.model,
                            tools: message.tools,
                            mcpServers: message.mcp_servers,
                            slashCommands: message.slash_commands,
                            skills: message.skills,
                            agents: message.agents,
                            permissionMode: message.permissionMode,
                            cwd: message.cwd,
                            claudeCodeVersion: message.claude_code_version,
                        });
                    }
                    else if ("subtype" in message && message.subtype === "compact_boundary") {
                        const m = message;
                        emit({
                            type: "compactBoundary",
                            sessionId: session.sessionId,
                            trigger: m.compact_metadata?.trigger ?? "manual",
                            preTokens: m.compact_metadata?.pre_tokens ?? 0,
                        });
                    }
                    break;
                }
                case "stream_event": {
                    const ev = message.event;
                    if (ev?.type === "content_block_start") {
                        if (ev.content_block?.type === "text") {
                            assistantItemId = `msg_${cryptoRandom()}`;
                        }
                        else if (ev.content_block?.type === "thinking") {
                            reasoningItemId = `rsn_${cryptoRandom()}`;
                        }
                    }
                    if (ev?.type === "content_block_delta") {
                        if (ev.delta?.type === "text_delta" && ev.delta.text) {
                            if (!assistantItemId)
                                assistantItemId = `msg_${cryptoRandom()}`;
                            emit({
                                type: "assistantDelta",
                                sessionId: session.sessionId,
                                turnId: session.activeTurnId ?? "",
                                itemId: assistantItemId,
                                delta: ev.delta.text,
                            });
                        }
                        else if (ev.delta?.type === "thinking_delta" && ev.delta.thinking) {
                            if (!reasoningItemId)
                                reasoningItemId = `rsn_${cryptoRandom()}`;
                            emit({
                                type: "reasoningDelta",
                                sessionId: session.sessionId,
                                turnId: session.activeTurnId ?? "",
                                itemId: reasoningItemId,
                                delta: ev.delta.thinking,
                            });
                        }
                    }
                    if (ev?.type === "content_block_stop") {
                        // Boundary — let the final assistant/result message carry the full text.
                    }
                    break;
                }
                case "assistant": {
                    const am = message;
                    if (am.error) {
                        emit({
                            type: "modelRerouted",
                            sessionId: session.sessionId,
                            reason: am.error,
                        });
                    }
                    const blocks = am.message?.content ?? [];
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
                        }
                        else if (block.type === "tool_use") {
                            emit({
                                type: "toolUse",
                                sessionId: session.sessionId,
                                turnId: session.activeTurnId ?? "",
                                toolUseId: block.id ?? `tu_${cryptoRandom()}`,
                                name: block.name ?? "unknown",
                                input: block.input,
                            });
                        }
                        else if (block.type === "thinking" && block.thinking) {
                            emit({
                                type: "reasoning",
                                sessionId: session.sessionId,
                                turnId: session.activeTurnId ?? "",
                                itemId: reasoningItemId ?? `rsn_${cryptoRandom()}`,
                                text: block.thinking,
                            });
                            reasoningItemId = null;
                        }
                    }
                    if (am.message?.usage) {
                        emit({
                            type: "tokenUsageUpdated",
                            sessionId: session.sessionId,
                            turnId: session.activeTurnId ?? "",
                            usage: am.message.usage,
                        });
                    }
                    break;
                }
                case "user": {
                    const blocks = (message.message?.content ?? []);
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
                    const r = message;
                    emit({
                        type: "turnCompleted",
                        sessionId: session.sessionId,
                        turnId,
                        isError: r.is_error,
                        usage: r.usage,
                        modelUsage: r.modelUsage,
                        totalCostUsd: r.total_cost_usd,
                        durationMs: r.duration_ms,
                        numTurns: r.num_turns,
                        result: r.result,
                        errors: r.errors,
                        permissionDenials: r.permission_denials,
                        structuredOutput: r.structured_output,
                    });
                    session.activeTurnId = null;
                    assistantItemId = null;
                    reasoningItemId = null;
                    break;
                }
            }
        }
    }
    catch (e) {
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
    }
    finally {
        session.query = null;
        emit({ type: "sessionClosed", sessionId: session.sessionId });
        sessions.delete(session.sessionId);
        // Reject any pending bridge callbacks so the SDK does not hang.
        for (const [, p] of session.pendingPermissions)
            p.reject(new Error("session closed"));
        for (const [, h] of session.pendingHooks)
            h.reject(new Error("session closed"));
    }
}
function cryptoRandom() {
    return Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
}
async function requireQuery(session) {
    if (session.query)
        return session.query;
    // Stream loop sets session.query right after `query({})` returns — wait
    // up to 2s for it.
    const start = Date.now();
    while (!session.query && Date.now() - start < 2000) {
        await new Promise((r) => setTimeout(r, 10));
    }
    if (!session.query)
        throw new Error("query handle not ready");
    return session.query;
}
function handleCommand(cmd) {
    void handleCommandAsync(cmd).catch((e) => {
        const err = e instanceof Error ? e.message : String(e);
        if ("id" in cmd && typeof cmd.id === "string") {
            emit({ type: "ack", id: cmd.id, ok: false, error: err });
        }
        else {
            log("error", `command failed: ${err}`);
        }
    });
}
async function handleCommandAsync(cmd) {
    switch (cmd.type) {
        case "createSession": {
            if (sessions.has(cmd.sessionId)) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "session already exists" });
                return;
            }
            const session = {
                sessionId: cmd.sessionId,
                input: new SessionInputBuffer(),
                abort: new AbortController(),
                activeTurnId: null,
                options: cmd.options ?? {},
                loopPromise: Promise.resolve(),
                sdkSessionId: null,
                query: null,
                cumulativeUsage: {},
                pendingPermissions: new Map(),
                pendingHooks: new Map(),
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
            session.input.push({
                type: "user",
                message: { role: "user", content: inputToBlocks(cmd.input) },
                parent_tool_use_id: null,
                session_id: session.sdkSessionId ?? session.sessionId,
            });
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
            session.input.push({
                type: "user",
                message: { role: "user", content: inputToBlocks(cmd.input) },
                parent_tool_use_id: null,
                session_id: session.sdkSessionId ?? session.sessionId,
            });
            emit({ type: "ack", id: cmd.id, ok: true });
            return;
        }
        case "injectItems": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            for (const raw of cmd.items) {
                if (raw && typeof raw === "object") {
                    session.input.push(raw);
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
            try {
                const q = await requireQuery(session);
                await q.interrupt();
                emit({ type: "ack", id: cmd.id, ok: true });
            }
            catch (e) {
                // Fall back to abort if interrupt() not available or fails.
                session.abort.abort();
                session.abort = new AbortController();
                emit({
                    type: "ack",
                    id: cmd.id,
                    ok: false,
                    error: `interrupt fallback to abort: ${e instanceof Error ? e.message : String(e)}`,
                });
            }
            return;
        }
        case "compact": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            // SDK has no programmatic compact API yet; push the user message
            // the CLI uses internally. PreCompact hook will fire if registered.
            session.input.push({
                type: "user",
                message: { role: "user", content: [{ type: "text", text: "/compact" }] },
                parent_tool_use_id: null,
                session_id: session.sdkSessionId ?? session.sessionId,
            });
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
        case "setModel": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            await q.setModel(cmd.model);
            emit({ type: "ack", id: cmd.id, ok: true });
            return;
        }
        case "setPermissionMode": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            // Cast: we accept any string at the wire boundary so new SDK modes
            // work without sidecar churn; the SDK validates on its end.
            await q.setPermissionMode(cmd.mode);
            emit({ type: "ack", id: cmd.id, ok: true });
            return;
        }
        case "setMaxThinkingTokens": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            await q.setMaxThinkingTokens(cmd.maxThinkingTokens);
            emit({ type: "ack", id: cmd.id, ok: true });
            return;
        }
        case "setMcpServers": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.setMcpServers(cmd.servers);
            emit({ type: "result", id: cmd.id, ok: true, payload: result });
            return;
        }
        case "rewindFiles": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.rewindFiles(cmd.userMessageId, cmd.dryRun !== undefined ? { dryRun: cmd.dryRun } : undefined);
            emit({ type: "result", id: cmd.id, ok: true, payload: result });
            return;
        }
        case "supportedCommands": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.supportedCommands();
            emit({ type: "result", id: cmd.id, ok: true, payload: { data: result } });
            return;
        }
        case "supportedModels": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.supportedModels();
            emit({ type: "result", id: cmd.id, ok: true, payload: { data: result } });
            return;
        }
        case "mcpServerStatus": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.mcpServerStatus();
            emit({ type: "result", id: cmd.id, ok: true, payload: { data: result } });
            return;
        }
        case "accountInfo": {
            const session = sessions.get(cmd.sessionId);
            if (!session) {
                emit({ type: "ack", id: cmd.id, ok: false, error: "no such session" });
                return;
            }
            const q = await requireQuery(session);
            const result = await q.accountInfo();
            emit({ type: "result", id: cmd.id, ok: true, payload: result });
            return;
        }
        case "permissionResponse": {
            // Find the session that owns this requestId — broadcast across all
            // since the Rust side knows the requestId but not necessarily the
            // sessionId until response time.
            for (const session of sessions.values()) {
                const p = session.pendingPermissions.get(cmd.requestId);
                if (!p)
                    continue;
                session.pendingPermissions.delete(cmd.requestId);
                const r = cmd.result;
                if (r.behavior === "allow") {
                    p.resolve({
                        behavior: "allow",
                        updatedInput: r.updatedInput ?? {},
                        updatedPermissions: r.updatedPermissions,
                        toolUseID: p.toolUseID,
                    });
                }
                else {
                    p.resolve({
                        behavior: "deny",
                        message: r.message,
                        interrupt: r.interrupt,
                        toolUseID: p.toolUseID,
                    });
                }
                emit({ type: "ack", id: cmd.id, ok: true });
                return;
            }
            emit({ type: "ack", id: cmd.id, ok: false, error: "unknown permission requestId" });
            return;
        }
        case "hookResponse": {
            for (const session of sessions.values()) {
                const h = session.pendingHooks.get(cmd.requestId);
                if (!h)
                    continue;
                session.pendingHooks.delete(cmd.requestId);
                h.resolve(cmd.output);
                emit({ type: "ack", id: cmd.id, ok: true });
                return;
            }
            emit({ type: "ack", id: cmd.id, ok: false, error: "unknown hook requestId" });
            return;
        }
        case "listSkills": {
            // Filesystem scan — same as before, but if a session exists, also
            // merge `Query.supportedCommands()` so SDK-loaded slash commands
            // appear too. We fall back to fs scan when no session is open yet.
            const fs = require("node:fs");
            const path = require("node:path");
            const cwds = cmd.cwds ?? [];
            const homes = [process.env.CLAUDE_HOME ?? path.join(process.env.HOME ?? "", ".claude")];
            const data = [];
            for (const root of [...homes, ...cwds.map((c) => path.join(c, ".claude"))]) {
                const skillsDir = path.join(root, "skills");
                if (!fs.existsSync(skillsDir))
                    continue;
                let entries;
                try {
                    entries = fs.readdirSync(skillsDir);
                }
                catch {
                    continue;
                }
                for (const entry of entries) {
                    const skillPath = path.join(skillsDir, entry, "SKILL.md");
                    if (!fs.existsSync(skillPath))
                        continue;
                    let description;
                    try {
                        const text = fs.readFileSync(skillPath, "utf8");
                        const match = /^description:\s*(.+)$/m.exec(text);
                        if (match)
                            description = match[1].trim();
                    }
                    catch { }
                    data.push({ name: entry, path: skillPath, description });
                }
            }
            emit({ type: "result", id: cmd.id, ok: true, payload: { data } });
            return;
        }
        case "listHooks": {
            // SDK uses programmatic hooks via Options.hooks; we surface only
            // an indication of which hook events are currently bridged.
            const data = [];
            for (const session of sessions.values()) {
                const sel = session.options.bridgeHooks;
                if (!sel)
                    continue;
                const evs = sel === "all" ? HOOK_EVENT_NAMES : (Array.isArray(sel) ? sel : []);
                for (const event of evs) {
                    data.push({ event, source: `session:${session.sessionId}` });
                }
            }
            emit({ type: "result", id: cmd.id, ok: true, payload: { data } });
            return;
        }
        case "listMcpServers": {
            // Prefer Query.mcpServerStatus() when a session is open.
            if (cmd.sessionId) {
                const session = sessions.get(cmd.sessionId);
                if (session && session.query) {
                    try {
                        const result = await session.query.mcpServerStatus();
                        emit({ type: "result", id: cmd.id, ok: true, payload: { data: result } });
                        return;
                    }
                    catch (e) {
                        log("warn", `mcpServerStatus failed: ${e instanceof Error ? e.message : String(e)}`);
                    }
                }
            }
            // Fallback: scan all sessions and aggregate.
            const seen = new Map();
            for (const session of sessions.values()) {
                if (!session.query)
                    continue;
                try {
                    const list = await session.query.mcpServerStatus();
                    for (const s of list) {
                        seen.set(s.name, { name: s.name, status: s.status });
                    }
                }
                catch { }
            }
            emit({ type: "result", id: cmd.id, ok: true, payload: { data: [...seen.values()] } });
            return;
        }
        case "callMcpTool": {
            // The SDK does not expose a direct MCP tool invocation; tool calls
            // go through the model loop. Approximate by returning a clear error
            // so the Rust side can convey it without dropping the request.
            emit({
                type: "ack",
                id: cmd.id,
                ok: false,
                error: "mcpServer/tool/call: not supported by Claude Agent SDK (tools dispatch through turn/start)",
            });
            return;
        }
        case "shutdown": {
            emit({ type: "ack", id: cmd.id, ok: true });
            for (const session of sessions.values()) {
                session.abort.abort();
                session.input.close();
            }
            setTimeout(() => process.exit(0), 50);
            return;
        }
    }
}
// --- Entry point --------------------------------------------------------
function main() {
    process.stdin.on("error", (err) => {
        process.stderr.write(`stdin error: ${err}\n`);
    });
    const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
    rl.on("line", (line) => {
        const trimmed = line.trim();
        if (!trimmed)
            return;
        let parsed;
        try {
            parsed = JSON.parse(trimmed);
        }
        catch (e) {
            log("warn", `invalid command line: ${e.message}`);
            return;
        }
        try {
            handleCommand(parsed);
        }
        catch (e) {
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
//# sourceMappingURL=index.js.map