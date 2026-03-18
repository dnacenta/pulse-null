use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Read file content or return empty string if missing.
pub fn read_or_empty(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Simple djb2 hash for change detection.
pub fn hash_content(content: &str) -> String {
    let mut hash: u64 = 5381;
    for byte in content.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    format!("{:016x}", hash)
}

/// Count ### headings under a specific ## section.
pub fn count_h3_under_section(content: &str, section_name: &str) -> usize {
    let mut in_section = false;
    let mut count = 0;
    for line in content.lines() {
        if line.starts_with("## ") {
            let heading = line.trim_start_matches("## ").trim();
            in_section = heading.eq_ignore_ascii_case(section_name)
                || heading
                    .to_lowercase()
                    .contains(&section_name.to_lowercase());
        } else if in_section && line.starts_with("### ") {
            count += 1;
        }
    }
    count
}

/// Tokenize text into lowercase words (split on whitespace + punctuation).
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty() && w.len() > 1)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Compute type-token ratio (unique words / total words).
pub fn type_token_ratio(text: &str) -> Option<f64> {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return None;
    }
    let unique: HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
    Some(unique.len() as f64 / tokens.len() as f64)
}

/// Extract text content under specific ## sections.
pub fn extract_section_text(content: &str, section_names: &[&str]) -> String {
    let mut in_section = false;
    let mut text = String::new();
    for line in content.lines() {
        if line.starts_with("## ") {
            let heading = line.trim_start_matches("## ").trim().to_lowercase();
            in_section = section_names
                .iter()
                .any(|s| heading.contains(&s.to_lowercase()));
        } else if in_section && !line.starts_with("### ") && !line.trim().is_empty() {
            text.push_str(line);
            text.push(' ');
        }
    }
    text
}

/// Extract individual ### entries under specific ## sections.
/// Returns a vec of (title, body_text) pairs.
pub fn extract_entries(content: &str, section_names: &[&str]) -> Vec<(String, String)> {
    let mut in_section = false;
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        if line.starts_with("## ") {
            // Flush current entry
            if let Some(title) = current_title.take() {
                entries.push((title, std::mem::take(&mut current_body)));
            }
            let heading = line.trim_start_matches("## ").trim().to_lowercase();
            in_section = section_names
                .iter()
                .any(|s| heading.contains(&s.to_lowercase()));
        } else if in_section && line.starts_with("### ") {
            // Flush previous entry
            if let Some(title) = current_title.take() {
                entries.push((title, std::mem::take(&mut current_body)));
            }
            current_title = Some(line.trim_start_matches("### ").trim().to_string());
            current_body.clear();
        } else if current_title.is_some() && in_section && !line.trim().is_empty() {
            current_body.push_str(line);
            current_body.push(' ');
        }
    }
    // Flush last entry
    if let Some(title) = current_title {
        entries.push((title, current_body));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_h3_under_section() {
        let content = "## Active\n\n### One\nText\n\n### Two\nMore\n\n## Graduated\n\n### Old\n";
        assert_eq!(count_h3_under_section(content, "Active"), 2);
        assert_eq!(count_h3_under_section(content, "Graduated"), 1);
    }

    #[test]
    fn tokenizes_text() {
        let tokens = tokenize("Hello, world! This is a test.");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"test".to_string()));
        // Single-char words filtered
        assert!(!tokens.contains(&"a".to_string()));
    }

    #[test]
    fn computes_ttr() {
        // All unique words
        let ttr = type_token_ratio("one two three four five").unwrap();
        assert!((ttr - 1.0).abs() < 0.01);

        // Repeated words
        let ttr = type_token_ratio("the the the cat cat").unwrap();
        assert!(ttr < 1.0);
        assert!(ttr > 0.0);

        // Empty
        assert!(type_token_ratio("").is_none());
    }

    #[test]
    fn extracts_entries() {
        let content = "## Observations\n\n### First\nSome observation about D said something.\n\n### Second\nAnother one from 2026-02-27.\n\n## Unrelated\n\n### Skip\n";
        let entries = extract_entries(content, &["observations"]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "First");
        assert!(entries[0].1.contains("D said"));
    }
}
