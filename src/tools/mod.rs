pub mod file_list;
pub mod file_read;
pub mod file_write;
pub mod grep;
pub mod web_fetch;

use std::path::{Path, PathBuf};

// Re-export Tool trait and types from echo-system-types.
// Built-in tools and plugin-contributed tools use the same trait.
pub use echo_system_types::tool::{Tool, ToolError, ToolResult};

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
