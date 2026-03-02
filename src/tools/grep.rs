use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use globset::{Glob, GlobMatcher};
use regex::Regex;

use super::{resolve_sandboxed_path, Tool, ToolError, ToolResult};

/// Maximum number of matching lines returned.
const MAX_MATCHES: usize = 200;

/// Maximum file size to search (1 MB). Larger files are skipped.
const MAX_FILE_SIZE: u64 = 1_024 * 1_024;

/// Search file contents for a pattern within the entity's data directory.
pub struct GrepTool {
    entity_root: PathBuf,
}

impl GrepTool {
    pub fn new(entity_root: PathBuf) -> Self {
        Self { entity_root }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for a regex pattern within the entity's data directory. \
         Returns matching lines with file paths and line numbers."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file path relative to entity root. Defaults to root."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files, e.g. \"*.md\" or \"**/*.toml\""
                }
            },
            "required": ["pattern"]
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let entity_root = self.entity_root.clone();
        Box::pin(async move {
            let pattern_str = input["pattern"].as_str().ok_or_else(|| {
                ToolError::ExecutionFailed("Missing 'pattern' parameter".to_string())
            })?;

            let re = Regex::new(pattern_str)
                .map_err(|e| ToolError::ExecutionFailed(format!("Invalid regex pattern: {}", e)))?;

            let path = input["path"].as_str().unwrap_or(".");
            let resolved = resolve_sandboxed_path(&entity_root, path)?;

            if !resolved.exists() {
                return Err(ToolError::NotFound(format!("Path not found: {}", path)));
            }

            let glob_matcher = match input["glob"].as_str() {
                Some(glob_str) => {
                    let glob = Glob::new(glob_str).map_err(|e| {
                        ToolError::ExecutionFailed(format!("Invalid glob pattern: {}", e))
                    })?;
                    Some(glob.compile_matcher())
                }
                None => None,
            };

            // Collect files to search — run the blocking walk on a thread pool
            let files = tokio::task::spawn_blocking({
                let resolved = resolved.clone();
                let entity_root = entity_root.clone();
                let glob_matcher = glob_matcher.clone();
                move || collect_files(&resolved, &entity_root, glob_matcher.as_ref())
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Task join error: {}", e)))?;

            // Search each file — also blocking I/O
            let matches = tokio::task::spawn_blocking({
                let entity_root = entity_root.clone();
                move || search_files(&files, &re, &entity_root)
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Task join error: {}", e)))?;

            if matches.is_empty() {
                return Ok(format!("No matches found for pattern '{}'", pattern_str));
            }

            let total = matches.len();
            let truncated = total > MAX_MATCHES;
            let mut output: Vec<String> = matches.into_iter().take(MAX_MATCHES).collect();

            if truncated {
                output.push(format!(
                    "\n... truncated ({} total matches, showing first {})",
                    total, MAX_MATCHES
                ));
            }

            Ok(output.join("\n"))
        })
    }
}

/// Walk the directory tree and collect files to search.
fn collect_files(
    start: &PathBuf,
    entity_root: &PathBuf,
    glob_matcher: Option<&GlobMatcher>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if start.is_file() {
        if should_search_file(start, entity_root, glob_matcher) {
            files.push(start.clone());
        }
        return files;
    }

    for entry in walkdir::WalkDir::new(start)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && should_search_file(&path.to_path_buf(), entity_root, glob_matcher) {
            files.push(path.to_path_buf());
        }
    }

    files.sort();
    files
}

/// Check whether a file should be searched.
fn should_search_file(
    path: &PathBuf,
    entity_root: &PathBuf,
    glob_matcher: Option<&GlobMatcher>,
) -> bool {
    // Skip files over the size limit
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.len() > MAX_FILE_SIZE {
            return false;
        }
    }

    // Apply glob filter against the relative path
    if let Some(matcher) = glob_matcher {
        if let Ok(relative) = path.strip_prefix(entity_root) {
            if !matcher.is_match(relative) {
                return false;
            }
        } else {
            return false;
        }
    }

    // Skip likely binary files by extension
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let binary_exts = [
            "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "mp3", "mp4", "wav", "ogg",
            "flac", "avi", "mkv", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "bin", "exe",
            "dll", "so", "dylib", "o", "a", "wasm", "pdf", "db", "sqlite",
        ];
        if binary_exts.contains(&ext.to_lowercase().as_str()) {
            return false;
        }
    }

    true
}

/// Search files for the regex pattern and return formatted match lines.
fn search_files(files: &[PathBuf], re: &Regex, entity_root: &PathBuf) -> Vec<String> {
    let mut matches = Vec::new();

    for file_path in files {
        let file = match std::fs::File::open(file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let relative = file_path
            .strip_prefix(entity_root)
            .unwrap_or(file_path)
            .to_string_lossy();

        let reader = BufReader::new(file);
        for (line_num, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue, // skip unreadable lines (binary content)
            };

            if re.is_match(&line) {
                matches.push(format!("{}:{}:{}", relative, line_num + 1, line));

                // Early exit if we've collected way more than we'll show
                if matches.len() > MAX_MATCHES * 2 {
                    return matches;
                }
            }
        }
    }

    matches
}
