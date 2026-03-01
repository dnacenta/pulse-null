pub mod file_list;
pub mod file_read;
pub mod file_write;

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Result type for tool execution.
pub type ToolResult<'a> = Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;

/// Errors from tool execution.
#[derive(Debug)]
pub enum ToolError {
    NotFound(String),
    ExecutionFailed(String),
    PermissionDenied(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotFound(msg) => write!(f, "Not found: {}", msg),
            ToolError::ExecutionFailed(msg) => write!(f, "Execution failed: {}", msg),
            ToolError::PermissionDenied(msg) => write!(f, "Permission denied: {}", msg),
        }
    }
}

impl std::error::Error for ToolError {}

/// A tool that can be invoked by an LLM.
pub trait Tool: Send + Sync {
    /// Tool name (must match what the LLM calls).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given input.
    fn execute(&self, input: serde_json::Value) -> ToolResult<'_>;
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Serialize all tool definitions for the Claude API request.
    pub fn definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema()
                })
            })
            .collect()
    }

    /// Whether the registry has any tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Resolve a relative path within the entity data directory.
/// Returns an error if the path tries to escape the sandbox.
pub fn resolve_sandboxed_path(
    entity_root: &Path,
    relative_path: &str,
) -> Result<PathBuf, ToolError> {
    // Reject absolute paths
    if relative_path.starts_with('/') || relative_path.starts_with('\\') {
        return Err(ToolError::PermissionDenied(
            "Absolute paths are not allowed".to_string(),
        ));
    }

    // Reject path traversal
    if relative_path.contains("..") {
        return Err(ToolError::PermissionDenied(
            "Path traversal (..) is not allowed".to_string(),
        ));
    }

    let resolved = entity_root.join(relative_path);

    // Extra safety: ensure the canonical path is still under entity_root.
    // We check the resolved path's prefix rather than canonicalizing (the file may not exist yet).
    if !resolved.starts_with(entity_root) {
        return Err(ToolError::PermissionDenied(
            "Path escapes entity data directory".to_string(),
        ));
    }

    Ok(resolved)
}
