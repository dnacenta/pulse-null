use std::path::Path;

use chrono::Utc;

use super::health::PipelineHealth;
use crate::config::PipelineConfig;

/// Check all documents and auto-archive any that hit their hard limit.
/// Returns a list of documents that were archived.
pub fn check_and_archive(
    root_dir: &Path,
    config: &PipelineConfig,
    health: &PipelineHealth,
) -> Vec<String> {
    let mut archived = Vec::new();

    if health.learning.count >= config.learning_hard
        && archive_document(root_dir, "journal/LEARNING.md", "archives/learning").is_ok()
    {
        archived.push("LEARNING.md".to_string());
    }
    if health.thoughts.count >= config.thoughts_hard
        && archive_document(root_dir, "journal/THOUGHTS.md", "archives/thoughts").is_ok()
    {
        archived.push("THOUGHTS.md".to_string());
    }
    if health.curiosity.count >= config.curiosity_hard
        && archive_document(root_dir, "journal/CURIOSITY.md", "archives/curiosity").is_ok()
    {
        archived.push("CURIOSITY.md".to_string());
    }
    if health.reflections.count >= config.reflections_hard
        && archive_document(root_dir, "journal/REFLECTIONS.md", "archives/reflections").is_ok()
    {
        archived.push("REFLECTIONS.md".to_string());
    }
    if health.praxis.count >= config.praxis_hard
        && archive_document(root_dir, "journal/PRAXIS.md", "archives/praxis").is_ok()
    {
        archived.push("PRAXIS.md".to_string());
    }

    archived
}

/// Archive a single document: move oldest entries to archive file, keep recent ones.
/// Strategy: split by ## headers, move the oldest half to archive, keep the newer half.
fn archive_document(
    root_dir: &Path,
    source_rel: &str,
    archive_dir_rel: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let source_path = root_dir.join(source_rel);
    let archive_dir = root_dir.join(archive_dir_rel);
    std::fs::create_dir_all(&archive_dir)?;

    let content = std::fs::read_to_string(&source_path)?;
    let (header, sections) = split_by_headers(&content);

    if sections.is_empty() {
        return Ok(());
    }

    // Move the oldest half to archive
    let split_point = sections.len() / 2;
    let (to_archive, to_keep) = sections.split_at(split_point);

    if to_archive.is_empty() {
        return Ok(());
    }

    // Write archived content
    let date = Utc::now().format("%Y-%m-%d");
    let archive_file = archive_dir.join(format!("archive-{}.md", date));

    let archive_content = if archive_file.exists() {
        let existing = std::fs::read_to_string(&archive_file)?;
        format!("{}\n{}", existing, to_archive.join("\n"))
    } else {
        let doc_name = source_rel.rsplit('/').next().unwrap_or(source_rel);
        format!(
            "# Archive — {} ({})\n\n{}",
            doc_name,
            date,
            to_archive.join("\n")
        )
    };
    std::fs::write(&archive_file, archive_content)?;

    // Rewrite source with header + remaining entries
    let new_content = format!("{}\n{}", header, to_keep.join("\n"));
    std::fs::write(&source_path, new_content)?;

    tracing::info!(
        "Archived {} entries from {} to {}",
        to_archive.len(),
        source_rel,
        archive_dir_rel,
    );

    Ok(())
}

/// Manually archive a specific document (for CLI use)
pub fn archive_document_by_name(
    root_dir: &Path,
    document: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let (source, archive_dir) = match document.to_lowercase().as_str() {
        "learning" => ("journal/LEARNING.md", "archives/learning"),
        "thoughts" => ("journal/THOUGHTS.md", "archives/thoughts"),
        "curiosity" => ("journal/CURIOSITY.md", "archives/curiosity"),
        "reflections" => ("journal/REFLECTIONS.md", "archives/reflections"),
        "praxis" => ("journal/PRAXIS.md", "archives/praxis"),
        _ => {
            return Err(format!(
                "Unknown document: {}. Valid: learning, thoughts, curiosity, reflections, praxis",
                document
            )
            .into())
        }
    };

    archive_document(root_dir, source, archive_dir)?;
    Ok(format!("Archived entries from {}", source))
}

/// Split markdown content into a header (everything before first ##) and sections (each starting with ##)
fn split_by_headers(content: &str) -> (String, Vec<String>) {
    let mut header = String::new();
    let mut sections: Vec<String> = Vec::new();
    let mut current_section = String::new();
    let mut in_header = true;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("## ") || trimmed.starts_with("### "))
            && !super::health::is_structural_header(trimmed)
        {
            if in_header {
                in_header = false;
            } else if !current_section.is_empty() {
                sections.push(current_section.clone());
            }
            current_section = format!("{}\n", line);
        } else if in_header {
            header.push_str(line);
            header.push('\n');
        } else {
            current_section.push_str(line);
            current_section.push('\n');
        }
    }

    if !current_section.is_empty() {
        sections.push(current_section);
    }

    (header, sections)
}

/// List archived files for a document type
pub fn list_archives(
    root_dir: &Path,
    document: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let dirs: Vec<&str> = match document {
        Some(d) => match d.to_lowercase().as_str() {
            "learning" => vec!["archives/learning"],
            "thoughts" => vec!["archives/thoughts"],
            "curiosity" => vec!["archives/curiosity"],
            "reflections" => vec!["archives/reflections"],
            "praxis" => vec!["archives/praxis"],
            _ => return Err(format!("Unknown document: {}", d).into()),
        },
        None => vec![
            "archives/learning",
            "archives/thoughts",
            "archives/curiosity",
            "archives/reflections",
            "archives/praxis",
        ],
    };

    let mut files = Vec::new();
    for dir in dirs {
        let path = root_dir.join(dir);
        if path.exists() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.ends_with(".md") {
                            files.push(format!("{}/{}", dir, name));
                        }
                    }
                }
            }
        }
    }

    files.sort();
    Ok(files)
}

// Make is_structural_header accessible from here (it's pub(crate) in health.rs)
// We reference it via super::health::is_structural_header

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_split_by_headers() {
        let content =
            "# Title\n\nPreamble.\n\n## Entry 1\n\nContent 1.\n\n## Entry 2\n\nContent 2.\n";
        let (header, sections) = split_by_headers(content);
        assert!(header.contains("Title"));
        assert!(header.contains("Preamble"));
        assert_eq!(sections.len(), 2);
        assert!(sections[0].contains("Entry 1"));
        assert!(sections[1].contains("Entry 2"));
    }

    #[test]
    fn test_archive_document() {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("journal");
        let archives = dir.path().join("archives/learning");
        fs::create_dir_all(&journal).unwrap();
        fs::create_dir_all(&archives).unwrap();

        fs::write(
            journal.join("LEARNING.md"),
            "# Learning\n\nPreamble.\n\n## Topic 1\n\nOld content.\n\n## Topic 2\n\nOlder content.\n\n## Topic 3\n\nNew content.\n\n## Topic 4\n\nNewest content.\n",
        ).unwrap();

        archive_document(dir.path(), "journal/LEARNING.md", "archives/learning").unwrap();

        // Check source was trimmed
        let remaining = fs::read_to_string(journal.join("LEARNING.md")).unwrap();
        let (_, sections) = split_by_headers(&remaining);
        assert_eq!(sections.len(), 2); // kept newer half

        // Check archive was created
        let archive_files: Vec<_> = fs::read_dir(&archives).unwrap().flatten().collect();
        assert_eq!(archive_files.len(), 1);
    }

    #[test]
    fn test_list_archives_empty() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("archives/learning")).unwrap();
        let files = list_archives(dir.path(), Some("learning")).unwrap();
        assert!(files.is_empty());
    }
}
