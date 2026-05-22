//! Method name constants. Match codex method names so clients can reuse schemas.

pub mod request {
    // Lifecycle
    pub const INITIALIZE: &str = "initialize";

    // Threads
    pub const THREAD_START: &str = "thread/start";
    pub const THREAD_RESUME: &str = "thread/resume";
    pub const THREAD_FORK: &str = "thread/fork";
    pub const THREAD_LIST: &str = "thread/list";
    pub const THREAD_LOADED_LIST: &str = "thread/loaded/list";
    pub const THREAD_READ: &str = "thread/read";
    pub const THREAD_ARCHIVE: &str = "thread/archive";
    pub const THREAD_UNARCHIVE: &str = "thread/unarchive";
    pub const THREAD_UNSUBSCRIBE: &str = "thread/unsubscribe";
    pub const THREAD_NAME_SET: &str = "thread/name/set";
    pub const THREAD_INJECT_ITEMS: &str = "thread/inject_items";
    pub const THREAD_COMPACT_START: &str = "thread/compact/start";

    // Thread goals
    pub const THREAD_GOAL_SET: &str = "thread/goal/set";
    pub const THREAD_GOAL_GET: &str = "thread/goal/get";
    pub const THREAD_GOAL_CLEAR: &str = "thread/goal/clear";

    // Turns
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const TURN_STEER: &str = "turn/steer";

    // Models / config
    pub const MODEL_LIST: &str = "model/list";
    pub const CONFIG_READ: &str = "config/read";

    // MCP
    pub const MCP_SERVER_STATUS_LIST: &str = "mcpServerStatus/list";
    pub const MCP_SERVER_TOOL_CALL: &str = "mcpServer/tool/call";
    pub const MCP_SERVER_RESOURCE_READ: &str = "mcpServer/resource/read";

    // Skills / hooks
    pub const SKILLS_LIST: &str = "skills/list";
    pub const HOOKS_LIST: &str = "hooks/list";

    // Filesystem
    pub const FS_READ_FILE: &str = "fs/readFile";
    pub const FS_WRITE_FILE: &str = "fs/writeFile";
    pub const FS_CREATE_DIRECTORY: &str = "fs/createDirectory";
    pub const FS_GET_METADATA: &str = "fs/getMetadata";
    pub const FS_READ_DIRECTORY: &str = "fs/readDirectory";
    pub const FS_REMOVE: &str = "fs/remove";
    pub const FS_COPY: &str = "fs/copy";
    pub const FS_WATCH: &str = "fs/watch";
    pub const FS_UNWATCH: &str = "fs/unwatch";

    // Process / command exec
    pub const COMMAND_EXEC: &str = "command/exec";
    pub const COMMAND_EXEC_WRITE: &str = "command/exec/write";
    pub const COMMAND_EXEC_TERMINATE: &str = "command/exec/terminate";

    // Pagination / metadata
    pub const THREAD_TURNS_LIST: &str = "thread/turns/list";
    pub const THREAD_TURNS_ITEMS_LIST: &str = "thread/turns/items/list";
    pub const THREAD_METADATA_UPDATE: &str = "thread/metadata/update";
    pub const THREAD_SETTINGS_UPDATE: &str = "thread/settings/update";
    pub const THREAD_ROLLBACK: &str = "thread/rollback";
    pub const THREAD_SHELL_COMMAND: &str = "thread/shellCommand";
    pub const THREAD_BACKGROUND_TERMINALS_CLEAN: &str = "thread/backgroundTerminals/clean";
    pub const THREAD_MEMORY_MODE_SET: &str = "thread/memoryMode/set";
    pub const MEMORY_RESET: &str = "memory/reset";

    // Capability lists
    pub const PERMISSION_PROFILE_LIST: &str = "permissionProfile/list";
    pub const EXPERIMENTAL_FEATURE_LIST: &str = "experimentalFeature/list";
    pub const COLLABORATION_MODE_LIST: &str = "collaborationMode/list";
    pub const MODEL_PROVIDER_CAPABILITIES_READ: &str = "modelProvider/capabilities/read";

    // Review
    pub const REVIEW_START: &str = "review/start";
}

pub mod notification {
    // Lifecycle
    pub const INITIALIZED: &str = "initialized";

    // Threads
    pub const THREAD_STARTED: &str = "thread/started";
    pub const THREAD_STATUS_CHANGED: &str = "thread/status/changed";
    pub const THREAD_CLOSED: &str = "thread/closed";
    pub const THREAD_ARCHIVED: &str = "thread/archived";
    pub const THREAD_UNARCHIVED: &str = "thread/unarchived";
    pub const THREAD_NAME_UPDATED: &str = "thread/name/updated";
    pub const THREAD_GOAL_UPDATED: &str = "thread/goal/updated";
    pub const THREAD_GOAL_CLEARED: &str = "thread/goal/cleared";

    // Turns
    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";

    // Items
    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const ITEM_AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";

    // Filesystem
    pub const FS_CHANGED: &str = "fs/changed";

    // Exec
    pub const COMMAND_EXEC_OUTPUT_DELTA: &str = "command/exec/outputDelta";

    // v0.4.0 thread mutators
    pub const THREAD_METADATA_UPDATED: &str = "thread/metadata/updated";
    pub const THREAD_SETTINGS_UPDATED: &str = "thread/settings/updated";
    pub const THREAD_MEMORY_MODE_CHANGED: &str = "thread/memoryMode/changed";
}
