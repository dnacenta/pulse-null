use std::path::PathBuf;

use super::{resolve_sandboxed_path, Tool, ToolError, ToolResult};

/// Write content to a file in the entity's data directory.
pub struct FileWriteTool {
    entity_root: PathBuf,
}

impl FileWriteTool {
    pub fn new(entity_root: PathBuf) -> Self {
        Self { entity_root }
    }
}

impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file in the entity's data directory. Creates parent directories if needed."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the entity's data directory"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let entity_root = self.entity_root.clone();
        Box::pin(async move {
            let path = input["path"].as_str().ok_or_else(|| {
                ToolError::ExecutionFailed("Missing 'path' parameter".to_string())
            })?;

            let content = input["content"].as_str().ok_or_else(|| {
                ToolError::ExecutionFailed("Missing 'content' parameter".to_string())
            })?;

            let resolved = resolve_sandboxed_path(&entity_root, path)?;

            // Create parent directories if they don't exist
            if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!(
                        "Failed to create directories for '{}': {}",
                        path, e
                    ))
                })?;
            }

            let bytes = content.len();
            tokio::fs::write(&resolved, content).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to write '{}': {}", path, e))
            })?;

            Ok(format!("Written {} bytes to {}", bytes, path))
        })
    }
}
