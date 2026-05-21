//! JSON-RPC error codes. Standard ones + codex-style extension `-32001`.

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub const SERVER_OVERLOADED: i64 = -32001;
pub const NOT_INITIALIZED: i64 = -32002;
pub const ALREADY_INITIALIZED: i64 = -32003;
