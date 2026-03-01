use std::path::PathBuf;

use super::{resolve_sandboxed_path, Tool, ToolError, ToolResult};

/// List files in a directory within the entity's data directory.
pub struct FileListTool {
    entity_root: PathBuf,
}

impl FileListTool {
    pub fn new(entity_root: PathBuf) -> Self {
        Self { entity_root }
    }
}

impl Tool for FileListTool {
    fn name(&self) -> &str {
        "file_list"
    }

    fn description(&self) -> &str {
        "List files and directories within the entity's data directory"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the entity's data directory. Defaults to the root."
                }
            },
            "required": []
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let entity_root = self.entity_root.clone();
        Box::pin(async move {
            let path = input["path"].as_str().unwrap_or(".");

            let resolved = resolve_sandboxed_path(&entity_root, path)?;

            if !resolved.exists() {
                return Err(ToolError::NotFound(format!(
                    "Directory not found: {}",
                    path
                )));
            }

            if !resolved.is_dir() {
                return Err(ToolError::ExecutionFailed(format!(
                    "'{}' is not a directory",
                    path
                )));
            }

            let mut entries = Vec::new();
            let mut read_dir = tokio::fs::read_dir(&resolved).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to read directory '{}': {}", path, e))
            })?;

            while let Some(entry) = read_dir
                .next_entry()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to read entry: {}", e)))?
            {
                let name = entry.file_name().to_string_lossy().to_string();
                let file_type = entry.file_type().await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to get file type: {}", e))
                })?;

                if file_type.is_dir() {
                    entries.push(format!("{}/", name));
                } else {
                    entries.push(name);
                }
            }

            entries.sort();
            Ok(entries.join("\n"))
        })
    }
}
