use std::path::PathBuf;

use super::{resolve_sandboxed_path, Tool, ToolError, ToolResult};

/// Read a file from the entity's data directory.
pub struct FileReadTool {
    entity_root: PathBuf,
}

impl FileReadTool {
    pub fn new(entity_root: PathBuf) -> Self {
        Self { entity_root }
    }
}

impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read a file from the entity's data directory"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the entity's data directory"
                }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let entity_root = self.entity_root.clone();
        Box::pin(async move {
            let path = input["path"].as_str().ok_or_else(|| {
                ToolError::ExecutionFailed("Missing 'path' parameter".to_string())
            })?;

            let resolved = resolve_sandboxed_path(&entity_root, path)?;

            if !resolved.exists() {
                return Err(ToolError::NotFound(format!("File not found: {}", path)));
            }

            if !resolved.is_file() {
                return Err(ToolError::ExecutionFailed(format!(
                    "'{}' is not a file",
                    path
                )));
            }

            tokio::fs::read_to_string(&resolved).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to read '{}': {}", path, e))
            })
        })
    }
}
