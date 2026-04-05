mod compact;
mod micro_compact;
mod tokens;

// Re-export public API — all external callers continue to work unchanged.
pub use compact::{compact_if_needed, extract_file_accesses, CompactionParams, CompactionResult};
pub use micro_compact::{micro_compact, truncate_tool_result_blocks, MicroCompactResult};
pub use tokens::{estimate_conversation_tokens, estimate_message_tokens};
