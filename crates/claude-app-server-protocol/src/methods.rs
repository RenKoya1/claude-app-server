//! Method name constants. Match codex method names so clients can reuse schemas.

pub mod request {
    pub const INITIALIZE: &str = "initialize";
    pub const THREAD_START: &str = "thread/start";
    pub const THREAD_RESUME: &str = "thread/resume";
    pub const THREAD_FORK: &str = "thread/fork";
    pub const THREAD_LIST: &str = "thread/list";
    pub const THREAD_READ: &str = "thread/read";
    pub const THREAD_ARCHIVE: &str = "thread/archive";
    pub const THREAD_UNSUBSCRIBE: &str = "thread/unsubscribe";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const TURN_STEER: &str = "turn/steer";
    pub const MODEL_LIST: &str = "model/list";
}

pub mod notification {
    pub const INITIALIZED: &str = "initialized";
    pub const THREAD_STARTED: &str = "thread/started";
    pub const THREAD_STATUS_CHANGED: &str = "thread/status/changed";
    pub const THREAD_CLOSED: &str = "thread/closed";
    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const ITEM_AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
}
