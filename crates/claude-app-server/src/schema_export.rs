//! Schema export. `claude-app-server generate-ts` / `generate-json-schema`.
//!
//! Codex generates these via ts-rs / schemars derives across its full type
//! graph. We approximate by emitting a hand-curated TypeScript and JSON
//! Schema bundle that covers the methods + payload types this server
//! actually implements. The output is regenerated per-version so clients
//! can `npx ... generate-ts --out src/types` after each upgrade.

use std::fs;
use std::path::Path;

pub fn write_typescript(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("claude-app-server.d.ts"), TS_BUNDLE)?;
    Ok(())
}

pub fn write_json_schema(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("claude-app-server.schema.json"), JSON_SCHEMA_BUNDLE)?;
    Ok(())
}

const TS_BUNDLE: &str = r#"/**
 * @renkoya1/claude-app-server — generated TypeScript bindings.
 *
 * Mirrors the JSON-RPC surface served by claude-app-server. Regenerate
 * after upgrading the server:
 *   npx @renkoya1/claude-app-server generate-ts --out src/types
 */

// ---------- JSON-RPC envelope ----------

export type RequestId = string | number;

export interface JsonRpcRequest<P = unknown> {
  id: RequestId;
  method: string;
  params?: P;
}

export interface JsonRpcNotification<P = unknown> {
  method: string;
  params?: P;
}

export interface JsonRpcResponse<R = unknown> {
  id: RequestId;
  result: R;
}

export interface JsonRpcError {
  id: RequestId;
  error: { code: number; message: string; data?: unknown };
}

export type JsonRpcMessage =
  | JsonRpcRequest
  | JsonRpcNotification
  | JsonRpcResponse
  | JsonRpcError;

// ---------- Core models ----------

export type ThreadStatus =
  | { type: "notLoaded" }
  | { type: "idle" }
  | { type: "systemError" }
  | { type: "active"; activeFlags: string[] };

export type TurnStatus = "inProgress" | "completed" | "interrupted" | "failed";

export interface Thread {
  id: string;
  preview: string;
  modelProvider: string;
  createdAt: number;
  updatedAt?: number;
  ephemeral?: boolean;
  path?: string;
  sessionId?: string;
  forkedFromId?: string;
  status: ThreadStatus;
  turns: Turn[];
}

export interface Turn {
  id: string;
  status: TurnStatus;
  items: Item[];
  error?: string;
}

export type Item =
  | { type: "userMessage"; id: string; content: InputChunk[] }
  | { type: "agentMessage"; id: string; text: string }
  | { type: "reasoning"; id: string; text: string }
  | { type: "toolUse"; id: string; toolUseId: string; name: string; input: unknown }
  | {
      type: "toolResult";
      id: string;
      toolUseId: string;
      content: unknown;
      isError: boolean;
    };

export type InputChunk =
  | { type: "text"; text: string }
  | { type: "image"; url: string }
  | { type: "localImage"; path: string };

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadInputTokens?: number;
  cacheCreationInputTokens?: number;
}

// ---------- Initialize ----------

export interface InitializeParams {
  clientInfo: { name: string; title?: string; version: string };
  capabilities?: {
    experimentalApi?: boolean;
    optOutNotificationMethods?: string[];
  };
}

export interface InitializeResult {
  userAgent: string;
  codexHome: string;
  platformFamily: string;
  platformOs: string;
}

// ---------- Threads ----------

/**
 * thread/start params. Common ergonomic fields are typed; anything else
 * is forwarded VERBATIM into `@anthropic-ai/claude-agent-sdk`
 * `query({ options })`. This is intentional — pass any SDK option to
 * customize your agent (allowedTools, disallowedTools, permissionMode,
 * maxTurns, mcpServers, agents, additionalDirectories, env, ...) and
 * future SDK options will work without a protocol release.
 */
export interface ThreadStartParams {
  model?: string;
  cwd?: string;
  ephemeral?: boolean;
  personality?: string;
  systemPrompt?: string;
  /**
   * Any other key is forwarded directly to the SDK as a query() option.
   * See https://github.com/anthropics/claude-agent-sdk-typescript for
   * the full Options type.
   */
  [sdkOption: string]: unknown;
}
export interface ThreadStartResult { thread: Thread; }

export interface ThreadResumeParams {
  threadId: string;
  model?: string;
  excludeTurns?: boolean;
}
export interface ThreadForkParams { threadId: string; ephemeral?: boolean; }
export interface ThreadReadParams { threadId: string; includeTurns?: boolean; }
export interface ThreadReadResult { thread: Thread; }
export interface ThreadListParams { cursor?: string; limit?: number; }
export interface ThreadListResult {
  data: Thread[];
  nextCursor?: string;
  backwardsCursor?: string;
}
export interface ThreadLoadedListResult { data: string[]; }
export interface ThreadArchiveParams { threadId: string; }
export interface ThreadUnsubscribeParams { threadId: string; }
export interface ThreadUnsubscribeResult {
  status: "unsubscribed" | "notSubscribed" | "notLoaded";
}
export interface ThreadNameSetParams { threadId: string; name: string; }
export interface ThreadInjectItemsParams { threadId: string; items: unknown[]; }
export interface ThreadCompactStartParams { threadId: string; }

// ---------- Goals ----------

export interface ThreadGoal {
  threadId: string;
  objective: string;
  status: string;
  tokenBudget?: number;
  tokensUsed: number;
  timeUsedSeconds: number;
  createdAt: number;
  updatedAt: number;
}
export interface ThreadGoalSetParams {
  threadId: string;
  objective?: string;
  status?: string;
  tokenBudget?: number;
}
export interface ThreadGoalResult { goal: ThreadGoal | null; }
export interface ThreadGoalGetParams { threadId: string; }
export interface ThreadGoalClearParams { threadId: string; }
export interface ThreadGoalClearResult { cleared: boolean; }

// ---------- Turns ----------

/** turn/start. Same open-ended SDK options passthrough as ThreadStartParams. */
export interface TurnStartParams {
  threadId: string;
  input: InputChunk[];
  model?: string;
  cwd?: string;
  systemPrompt?: string;
  [sdkOption: string]: unknown;
}
export interface TurnStartResult { turn: Turn; }
export interface TurnInterruptParams { threadId: string; turnId: string; }
export interface TurnSteerParams {
  threadId: string;
  input: InputChunk[];
  expectedTurnId: string;
}
export interface TurnSteerResult { turnId: string; }

// ---------- Models / config ----------

export interface ModelDescriptor { id: string; displayName: string; hidden?: boolean; }
export interface ModelListResult { data: ModelDescriptor[]; }
export interface ConfigReadResult {
  model: string;
  claudeHome: string;
  platformFamily: string;
  platformOs: string;
  cwd?: string;
}

// ---------- Filesystem ----------

export interface FsReadFileParams { path: string; }
export interface FsReadFileResult { dataBase64: string; }
export interface FsWriteFileParams { path: string; dataBase64: string; }
export interface FsCreateDirectoryParams { path: string; recursive?: boolean; }
export interface FsMetadataParams { path: string; }
export interface FsMetadataResult {
  isDirectory: boolean;
  isFile: boolean;
  isSymlink: boolean;
  createdAtMs?: number;
  modifiedAtMs?: number;
  sizeBytes?: number;
}
export interface FsReadDirectoryParams { path: string; }
export interface FsDirEntry { fileName: string; isDirectory: boolean; isFile: boolean; }
export interface FsReadDirectoryResult { entries: FsDirEntry[]; }
export interface FsRemoveParams { path: string; recursive?: boolean; force?: boolean; }
export interface FsCopyParams { source: string; destination: string; recursive?: boolean; }
export interface FsWatchParams { path: string; watchId: string; }
export interface FsWatchResult { path: string; }
export interface FsUnwatchParams { watchId: string; }

// ---------- Command exec ----------

export interface CommandExecParams {
  command: string[];
  processId?: string;
  cwd?: string;
  env?: Record<string, string>;
  timeoutMs?: number;
  disableTimeout?: boolean;
  outputBytesCap?: number;
  disableOutputCap?: boolean;
  streamStdoutStderr?: boolean;
}
export interface CommandExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
  timedOut?: boolean;
}
export interface CommandExecWriteParams {
  processId: string;
  dataBase64?: string;
  closeStdin?: boolean;
}
export interface CommandExecTerminateParams { processId: string; }

// ---------- Notification payloads ----------

export interface ThreadStartedEvent { thread: Thread; }
export interface ThreadStatusChangedEvent { threadId: string; status: ThreadStatus; }
export interface ThreadClosedEvent { threadId: string; }
export interface TurnStartedEvent { threadId: string; turn: Turn; }
export interface TurnCompletedEvent {
  threadId: string;
  turn: Turn;
  tokenUsage?: TokenUsage;
}
export interface ItemStartedEvent { threadId: string; turnId: string; item: Item; }
export interface ItemCompletedEvent { threadId: string; turnId: string; item: Item; }
export interface ItemAgentMessageDeltaEvent {
  threadId: string;
  turnId: string;
  itemId: string;
  delta: string;
}
export interface FsChangedEvent { watchId: string; changedPaths: string[]; }
export interface CommandExecOutputDeltaEvent {
  processId: string;
  stream: "stdout" | "stderr";
  deltaBase64: string;
}

// ---------- Method registry ----------

export const Methods = {
  initialize: "initialize",
  threadStart: "thread/start",
  threadResume: "thread/resume",
  threadFork: "thread/fork",
  threadList: "thread/list",
  threadLoadedList: "thread/loaded/list",
  threadRead: "thread/read",
  threadArchive: "thread/archive",
  threadUnarchive: "thread/unarchive",
  threadUnsubscribe: "thread/unsubscribe",
  threadNameSet: "thread/name/set",
  threadInjectItems: "thread/inject_items",
  threadCompactStart: "thread/compact/start",
  threadGoalSet: "thread/goal/set",
  threadGoalGet: "thread/goal/get",
  threadGoalClear: "thread/goal/clear",
  turnStart: "turn/start",
  turnInterrupt: "turn/interrupt",
  turnSteer: "turn/steer",
  modelList: "model/list",
  configRead: "config/read",
  skillsList: "skills/list",
  hooksList: "hooks/list",
  mcpServerStatusList: "mcpServerStatus/list",
  mcpServerToolCall: "mcpServer/tool/call",
  fsReadFile: "fs/readFile",
  fsWriteFile: "fs/writeFile",
  fsCreateDirectory: "fs/createDirectory",
  fsGetMetadata: "fs/getMetadata",
  fsReadDirectory: "fs/readDirectory",
  fsRemove: "fs/remove",
  fsCopy: "fs/copy",
  fsWatch: "fs/watch",
  fsUnwatch: "fs/unwatch",
  commandExec: "command/exec",
  commandExecWrite: "command/exec/write",
  commandExecTerminate: "command/exec/terminate",
  threadTurnsList: "thread/turns/list",
  threadTurnsItemsList: "thread/turns/items/list",
  threadMetadataUpdate: "thread/metadata/update",
  threadSettingsUpdate: "thread/settings/update",
  threadRollback: "thread/rollback",
  threadShellCommand: "thread/shellCommand",
  threadBackgroundTerminalsClean: "thread/backgroundTerminals/clean",
  threadMemoryModeSet: "thread/memoryMode/set",
  memoryReset: "memory/reset",
  permissionProfileList: "permissionProfile/list",
  experimentalFeatureList: "experimentalFeature/list",
  collaborationModeList: "collaborationMode/list",
  modelProviderCapabilitiesRead: "modelProvider/capabilities/read",
  reviewStart: "review/start",
  accountRead: "account/read",
  permissionRespond: "permission/respond",
  hookRespond: "hook/respond",
  agentDefine: "agent/define",
  agentList: "agent/list",
  agentRemove: "agent/remove",
  mcpServerSet: "mcpServer/set",
  threadModelSet: "thread/model/set",
  threadMaxThinkingTokensSet: "thread/maxThinkingTokens/set",
} as const;

export const Notifications = {
  initialized: "initialized",
  threadStarted: "thread/started",
  threadStatusChanged: "thread/status/changed",
  threadClosed: "thread/closed",
  threadArchived: "thread/archived",
  threadUnarchived: "thread/unarchived",
  threadNameUpdated: "thread/name/updated",
  threadGoalUpdated: "thread/goal/updated",
  threadGoalCleared: "thread/goal/cleared",
  turnStarted: "turn/started",
  turnCompleted: "turn/completed",
  itemStarted: "item/started",
  itemCompleted: "item/completed",
  itemAgentMessageDelta: "item/agentMessage/delta",
  fsChanged: "fs/changed",
  commandExecOutputDelta: "command/exec/outputDelta",
  threadMetadataUpdated: "thread/metadata/updated",
  threadSettingsUpdated: "thread/settings/updated",
  threadMemoryModeChanged: "thread/memoryMode/changed",
  threadSessionInit: "thread/session/init",
  itemReasoningTextDelta: "item/reasoning/textDelta",
  threadTokenUsageUpdated: "thread/tokenUsage/updated",
  threadCompacted: "thread/compacted",
  modelRerouted: "model/rerouted",
  hookStarted: "hook/started",
  hookCompleted: "hook/completed",
  itemCommandExecutionRequestApproval: "item/commandExecution/requestApproval",
  itemFileChangeRequestApproval: "item/fileChange/requestApproval",
  itemPermissionsRequestApproval: "item/permissions/requestApproval",
  itemToolRequestUserInput: "item/tool/requestUserInput",
  mcpServerElicitationRequest: "mcpServer/elicitation/request",
  accountUpdated: "account/updated",
} as const;
"#;

const JSON_SCHEMA_BUNDLE: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "claude-app-server",
  "description": "JSON-RPC surface for @renkoya1/claude-app-server. Method registry only — payload schemas live in claude-app-server.d.ts. Regenerate via `npx @renkoya1/claude-app-server generate-json-schema --out DIR`.",
  "type": "object",
  "properties": {
    "methods": {
      "type": "array",
      "items": { "type": "string" },
      "examples": [
        [
          "initialize",
          "thread/start", "thread/resume", "thread/fork", "thread/list",
          "thread/loaded/list", "thread/read", "thread/archive",
          "thread/unarchive", "thread/unsubscribe", "thread/name/set",
          "thread/inject_items", "thread/compact/start",
          "thread/goal/set", "thread/goal/get", "thread/goal/clear",
          "turn/start", "turn/interrupt", "turn/steer",
          "model/list", "config/read",
          "skills/list", "hooks/list",
          "mcpServerStatus/list", "mcpServer/tool/call",
          "fs/readFile", "fs/writeFile", "fs/createDirectory",
          "fs/getMetadata", "fs/readDirectory", "fs/remove", "fs/copy",
          "fs/watch", "fs/unwatch",
          "command/exec", "command/exec/write", "command/exec/terminate"
        ]
      ]
    },
    "notifications": {
      "type": "array",
      "items": { "type": "string" },
      "examples": [
        [
          "initialized",
          "thread/started", "thread/status/changed", "thread/closed",
          "thread/archived", "thread/unarchived", "thread/name/updated",
          "thread/goal/updated", "thread/goal/cleared",
          "turn/started", "turn/completed",
          "item/started", "item/completed", "item/agentMessage/delta",
          "fs/changed",
          "command/exec/outputDelta"
        ]
      ]
    }
  }
}
"#;
