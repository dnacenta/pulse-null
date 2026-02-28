use std::path::Path;

/// Read EPHEMERAL.md and return its contents, then clear it
pub fn consume_ephemeral(root_dir: &Path) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let path = root_dir.join("memory/EPHEMERAL.md");
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(None);
    }

    // Clear the file after consuming
    std::fs::write(&path, "")?;

    Ok(Some(content))
}

/// Write session summary to EPHEMERAL.md
pub fn write_ephemeral(root_dir: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = root_dir.join("memory/EPHEMERAL.md");
    std::fs::write(&path, content)?;
    Ok(())
}

/// Append to MEMORY.md
pub fn update_memory(root_dir: &Path, entry: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = root_dir.join("memory/MEMORY.md");
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    content.push_str(entry);
    content.push('\n');
    std::fs::write(&path, content)?;
    Ok(())
}
