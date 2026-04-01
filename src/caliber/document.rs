//! CALIBER.md document parser and updater.
//!
//! Reads the existing CALIBER.md, updates Capability Map scores
//! using trajectory mining data, appends to Calibration Record
//! and Evolution Log, and writes back to disk.
//!
//! Sections managed by the miner:
//! - Capability Map (scores updated from data)
//! - Calibration Record (prediction errors appended)
//! - Evolution Log (changes appended)
//!
//! Sections NOT managed (manual/vigil only):
//! - Known Limitations
//! - Behavioral Patterns
//! - Abstention Policy
//! - Error Taxonomy

use std::path::Path;

use super::trajectory::TrajectoryReport;

/// A parsed capability entry from the Capability Map table.
#[derive(Debug, Clone)]
pub struct CapabilityEntry {
    pub domain: String,
    pub confidence: f64,
    pub evidence: String,
    pub sample_count: usize,
    pub last_calibrated: String,
}

/// A parsed CALIBER.md document.
#[derive(Debug)]
pub struct CaliberDocument {
    /// Raw header lines before the Capability Map.
    pub header: String,
    /// Parsed capability entries.
    pub capabilities: Vec<CapabilityEntry>,
    /// Content between Capability Map and Calibration Record.
    pub middle_sections: String,
    /// Existing calibration records (raw lines).
    pub calibration_records: Vec<String>,
    /// Content between Calibration Record and Evolution Log.
    pub post_calibration: String,
    /// Existing evolution log entries (raw lines).
    pub evolution_entries: Vec<String>,
    /// Content after Evolution Log.
    pub footer: String,
}

/// Parse a CALIBER.md file into structured sections.
pub fn parse_caliber_md(content: &str) -> CaliberDocument {
    let lines: Vec<&str> = content.lines().collect();

    // Find section boundaries
    let cap_map_idx = lines.iter().position(|l| l.trim() == "## Capability Map");
    let cal_record_idx = lines
        .iter()
        .position(|l| l.trim() == "## Calibration Record");
    let evo_log_idx = lines.iter().position(|l| l.trim() == "## Evolution Log");

    // Extract header (everything before Capability Map)
    let header_end = cap_map_idx.unwrap_or(0);
    let header = lines[..header_end].join("\n");

    // Parse Capability Map table
    let capabilities = if let Some(cap_start) = cap_map_idx {
        parse_capability_table(&lines, cap_start, cal_record_idx.or(evo_log_idx))
    } else {
        vec![]
    };

    // Middle sections (Known Limitations, Behavioral Patterns, etc.)
    let middle_start = cap_map_idx
        .map(|i| find_section_end(&lines, i, cal_record_idx.or(evo_log_idx)))
        .unwrap_or(header_end);
    let middle_end = cal_record_idx.unwrap_or_else(|| evo_log_idx.unwrap_or(lines.len()));
    let middle_sections = if middle_start < middle_end {
        lines[middle_start..middle_end].join("\n")
    } else {
        String::new()
    };

    // Parse Calibration Record
    let calibration_records = if let Some(cal_start) = cal_record_idx {
        parse_table_rows(&lines, cal_start, evo_log_idx)
    } else {
        vec![]
    };

    // Post-calibration (between Calibration Record table and Evolution Log)
    let post_cal_start = cal_record_idx
        .map(|i| find_section_end(&lines, i, evo_log_idx))
        .unwrap_or(middle_end);
    let post_cal_end = evo_log_idx.unwrap_or(lines.len());
    let post_calibration = if post_cal_start < post_cal_end {
        lines[post_cal_start..post_cal_end].join("\n")
    } else {
        String::new()
    };

    // Parse Evolution Log
    let evolution_entries = if let Some(evo_start) = evo_log_idx {
        parse_table_rows(&lines, evo_start, None)
    } else {
        vec![]
    };

    // Footer (after Evolution Log)
    let footer_start = evo_log_idx
        .map(|i| find_section_end(&lines, i, None))
        .unwrap_or(lines.len());
    let footer = if footer_start < lines.len() {
        lines[footer_start..].join("\n")
    } else {
        String::new()
    };

    CaliberDocument {
        header,
        capabilities,
        middle_sections,
        calibration_records,
        post_calibration,
        evolution_entries,
        footer,
    }
}

/// Parse capability table rows into structured entries.
fn parse_capability_table(
    lines: &[&str],
    section_start: usize,
    next_section: Option<usize>,
) -> Vec<CapabilityEntry> {
    let mut entries = Vec::new();
    let end = next_section.unwrap_or(lines.len());

    // Skip header row and separator
    let mut in_table = false;
    let mut header_seen = false;

    for line in lines.iter().take(end).skip(section_start + 1) {
        let line = line.trim();

        // Empty line after table ends the table
        if in_table && line.is_empty() {
            break;
        }

        // Skip non-table lines
        if !line.starts_with('|') {
            continue;
        }

        // First table row is the header
        if !header_seen {
            header_seen = true;
            continue;
        }

        // Second row is the separator
        if line.contains("---") {
            in_table = true;
            continue;
        }

        if in_table {
            if let Some(entry) = parse_capability_row(line) {
                entries.push(entry);
            }
        }
    }

    entries
}

/// Parse a single capability table row.
fn parse_capability_row(line: &str) -> Option<CapabilityEntry> {
    let cells: Vec<&str> = line.split('|').collect();
    // Expected: | Domain | Confidence | Evidence | Sample | Last Calibrated |
    // cells[0] is empty (before first |), cells[1..6] are the values

    if cells.len() < 6 {
        return None;
    }

    let domain = cells[1].trim().to_string();
    let confidence_str = cells[2].trim();
    let evidence = cells[3].trim().to_string();
    let sample_str = cells[4].trim();
    let last_calibrated = cells[5].trim().to_string();

    let confidence = confidence_str.parse::<f64>().unwrap_or(0.5);

    // Parse sample count — handle formats like "~12 reflection sessions" or "12"
    let sample_count = sample_str
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<usize>()
        .unwrap_or(0);

    Some(CapabilityEntry {
        domain,
        confidence,
        evidence,
        sample_count,
        last_calibrated,
    })
}

/// Parse table data rows (for Calibration Record and Evolution Log).
fn parse_table_rows(
    lines: &[&str],
    section_start: usize,
    next_section: Option<usize>,
) -> Vec<String> {
    let mut rows = Vec::new();
    let end = next_section.unwrap_or(lines.len());

    let mut past_separator = false;

    for line in lines.iter().take(end).skip(section_start + 1) {
        let line = line.trim();

        if line.is_empty() && past_separator {
            break;
        }

        if !line.starts_with('|') {
            continue;
        }

        if line.contains("---") {
            past_separator = true;
            continue;
        }

        if past_separator {
            rows.push(line.to_string());
        }
    }

    rows
}

/// Find where a section's table content ends.
fn find_section_end(lines: &[&str], section_start: usize, next_section: Option<usize>) -> usize {
    let end = next_section.unwrap_or(lines.len());
    let mut past_table = false;

    for (i, line) in lines.iter().enumerate().take(end).skip(section_start + 1) {
        let line = line.trim();

        if line.starts_with('|') {
            past_table = true;
            continue;
        }

        // First non-table, non-empty line after table
        if past_table && !line.is_empty() {
            return i;
        }

        // Empty line after table
        if past_table && line.is_empty() {
            return i;
        }
    }

    end
}

/// Update CALIBER.md with trajectory mining data.
///
/// - Updates Capability Map scores using weighted average
/// - Appends prediction errors to Calibration Record
/// - Appends changes to Evolution Log
///
/// Returns true if any changes were made.
pub fn update_caliber_md(
    docs_dir: &Path,
    report: &TrajectoryReport,
) -> Result<bool, Box<dyn std::error::Error>> {
    let caliber_path = super::caliber_md(docs_dir);
    let content = std::fs::read_to_string(&caliber_path).unwrap_or_default();

    if content.is_empty() {
        // Create a fresh CALIBER.md from the report
        let new_content = create_caliber_md(report);
        std::fs::write(&caliber_path, new_content)?;
        return Ok(true);
    }

    let mut doc = parse_caliber_md(&content);
    let mut changed = false;

    // Update capability scores
    for dr in &report.domain_reports {
        // Only adjust if we have 3+ samples
        if dr.confidence_delta.is_none() {
            continue;
        }

        let today_success_rate = dr.success_rate;

        if let Some(entry) = doc
            .capabilities
            .iter_mut()
            .find(|c| normalize_domain(&c.domain) == normalize_domain(&dr.domain))
        {
            // Weighted average: new = (old * old_samples + today * today_samples) / total
            let old_score = entry.confidence;
            let old_samples = entry.sample_count.max(1) as f64;
            let today_samples = dr.sample_size as f64;
            let new_score = (old_score * old_samples + today_success_rate * today_samples)
                / (old_samples + today_samples);

            // Only update if delta is significant (>0.02)
            if (new_score - old_score).abs() > 0.02 {
                entry.confidence = (new_score * 100.0).round() / 100.0; // round to 2dp
                entry.sample_count += dr.sample_size;
                entry.last_calibrated = report.date.to_string();
                changed = true;
            }
        } else {
            // New domain not in the table — add it
            doc.capabilities.push(CapabilityEntry {
                domain: dr.domain.clone(),
                confidence: (today_success_rate * 100.0).round() / 100.0,
                evidence: format!("Auto-detected from {} outcome records.", dr.sample_size),
                sample_count: dr.sample_size,
                last_calibrated: report.date.to_string(),
            });
            changed = true;
        }
    }

    // Append prediction errors to Calibration Record
    for pe in &report.prediction_errors {
        let row = format!(
            "| {} | {} | {} ({}) | {} |",
            report.date, pe.predicted, pe.actual_outcome, pe.actual_valence, pe.error_type,
        );
        doc.calibration_records.push(row);
        changed = true;
    }

    // Append to Evolution Log if anything changed
    if changed {
        let changes: Vec<String> = report
            .domain_reports
            .iter()
            .filter(|dr| dr.confidence_delta.is_some())
            .map(|dr| format!("{}: {:.0}%", dr.domain, dr.success_rate * 100.0))
            .collect();
        let patterns_summary = if report.notable_patterns.is_empty() {
            "none".to_string()
        } else {
            report.notable_patterns.join("; ")
        };

        let entry = format!(
            "| {} | Trajectory mining: {} outcomes across {} domains | Auto-miner (caliber Phase 3) | Scores updated: {}. Patterns: {} |",
            report.date,
            report.total_outcomes,
            report.domain_reports.len(),
            if changes.is_empty() {
                "none (insufficient samples)".to_string()
            } else {
                changes.join(", ")
            },
            patterns_summary,
        );
        doc.evolution_entries.push(entry);
    }

    // Reconstruct the document
    if changed {
        let new_content = render_caliber_doc(&doc);
        std::fs::write(&caliber_path, new_content)?;
    }

    Ok(changed)
}

/// Normalize domain names for matching (lowercase, underscores to spaces).
fn normalize_domain(domain: &str) -> String {
    domain.to_lowercase().replace('_', " ")
}

/// Render a CaliberDocument back to markdown.
fn render_caliber_doc(doc: &CaliberDocument) -> String {
    let mut sections = Vec::new();

    // Header
    if !doc.header.is_empty() {
        sections.push(doc.header.clone());
    }

    // Capability Map
    sections.push("## Capability Map".to_string());
    sections.push(String::new());
    sections.push("| Domain | Confidence | Evidence | Sample | Last Calibrated |".to_string());
    sections.push("|---|---|---|---|---|".to_string());
    for cap in &doc.capabilities {
        sections.push(format!(
            "| {} | {:.2} | {} | {} | {} |",
            cap.domain, cap.confidence, cap.evidence, cap.sample_count, cap.last_calibrated,
        ));
    }

    // Middle sections (Known Limitations, Behavioral Patterns, etc.)
    if !doc.middle_sections.is_empty() {
        sections.push(String::new());
        sections.push(doc.middle_sections.clone());
    }

    // Calibration Record
    sections.push(String::new());
    sections.push("## Calibration Record".to_string());
    sections.push(String::new());
    sections.push("| Date | Prediction | Actual | Error |".to_string());
    sections.push("|---|---|---|---|".to_string());
    for row in &doc.calibration_records {
        sections.push(row.clone());
    }

    // Post-calibration content
    if !doc.post_calibration.is_empty() {
        sections.push(String::new());
        sections.push(doc.post_calibration.clone());
    }

    // Evolution Log
    sections.push(String::new());
    sections.push("## Evolution Log".to_string());
    sections.push(String::new());
    sections.push("| Date | Change | Trigger | Effect |".to_string());
    sections.push("|---|---|---|---|".to_string());
    for row in &doc.evolution_entries {
        sections.push(row.clone());
    }

    // Footer
    if !doc.footer.is_empty() {
        sections.push(String::new());
        sections.push(doc.footer.clone());
    }

    sections.push(String::new()); // trailing newline
    sections.join("\n")
}

/// Create a fresh CALIBER.md from a trajectory report.
fn create_caliber_md(report: &TrajectoryReport) -> String {
    let mut doc = CaliberDocument {
        header: format!(
            "# Echo — Caliber\n\n# Last updated: {} by trajectory miner (auto-generated)\n# Update frequency: nightly (trajectory mining)\n",
            report.date
        ),
        capabilities: vec![],
        middle_sections: String::new(),
        calibration_records: vec![],
        post_calibration: String::new(),
        evolution_entries: vec![],
        footer: String::new(),
    };

    // Populate capabilities from report
    for dr in &report.domain_reports {
        doc.capabilities.push(CapabilityEntry {
            domain: dr.domain.clone(),
            confidence: (dr.success_rate * 100.0).round() / 100.0,
            evidence: format!("Auto-bootstrapped from {} outcome records.", dr.sample_size),
            sample_count: dr.sample_size,
            last_calibrated: report.date.to_string(),
        });
    }

    // Add initial evolution entry
    doc.evolution_entries.push(format!(
        "| {} | CALIBER.md auto-created from trajectory mining | Auto-miner (caliber Phase 3) | {} domains, {} total outcomes |",
        report.date,
        report.domain_reports.len(),
        report.total_outcomes,
    ));

    render_caliber_doc(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caliber::outcome::{Outcome, Valence};
    use crate::caliber::trajectory::{
        DomainReport, PredictionErrorType, TrajectoryReport, ValenceDistribution,
    };
    use chrono::Utc;
    use tempfile::TempDir;

    fn sample_caliber_md() -> &'static str {
        r#"# Echo — Caliber

# Last updated: 2026-03-01 by Echo (bootstrap)

## Capability Map

| Domain | Confidence | Evidence | Sample | Last Calibrated |
|---|---|---|---|---|
| Philosophical reflection | 0.80 | 3 positions developed. | ~12 reflection sessions | 2026-03-01 (bootstrap) |
| Research synthesis | 0.80 | Strong at convergence. | ~6 research sessions | 2026-03-01 (bootstrap) |
| Rust implementation | 0.70 | echo-system built. | ~8 coding sessions | 2026-03-01 (bootstrap) |

## Known Limitations

- **Silent failure blindness**: I don't notice when my own systems break silently.

## Behavioral Patterns

- **Convergence seeking**: I look for unified frameworks.

## Calibration Record

| Date | Prediction | Actual | Error |
|---|---|---|---|
| 2026-03-01 | Self-model research will extend Feb 27 work | Fundamental reframe | Underestimated novelty |

## Error Taxonomy

- **Reflection hallucination**: Insight that produces no behavioral change.

## Abstention Policy

- **Infrastructure changes**: ALWAYS confirm with D first

## Evolution Log

| Date | Change | Trigger | Effect |
|---|---|---|---|
| 2026-03-01 | CALIBER.md created (bootstrap v1) | Research + D approval | Baseline established |
"#
    }

    #[test]
    fn parse_caliber_md_extracts_capabilities() {
        let doc = parse_caliber_md(sample_caliber_md());
        assert_eq!(doc.capabilities.len(), 3);
        assert_eq!(doc.capabilities[0].domain, "Philosophical reflection");
        assert!((doc.capabilities[0].confidence - 0.80).abs() < 0.01);
        assert_eq!(doc.capabilities[0].sample_count, 12);
    }

    #[test]
    fn parse_caliber_md_extracts_calibration_records() {
        let doc = parse_caliber_md(sample_caliber_md());
        assert_eq!(doc.calibration_records.len(), 1);
        assert!(doc.calibration_records[0].contains("Underestimated"));
    }

    #[test]
    fn parse_caliber_md_extracts_evolution_log() {
        let doc = parse_caliber_md(sample_caliber_md());
        assert_eq!(doc.evolution_entries.len(), 1);
        assert!(doc.evolution_entries[0].contains("bootstrap v1"));
    }

    #[test]
    fn parse_preserves_middle_sections() {
        let doc = parse_caliber_md(sample_caliber_md());
        assert!(doc.middle_sections.contains("Known Limitations"));
        assert!(doc.middle_sections.contains("Behavioral Patterns"));
    }

    #[test]
    fn roundtrip_parse_render() {
        let original = sample_caliber_md();
        let doc = parse_caliber_md(original);
        let rendered = render_caliber_doc(&doc);

        // Check key content is preserved
        assert!(rendered.contains("Philosophical reflection"));
        assert!(rendered.contains("0.80"));
        assert!(rendered.contains("Known Limitations"));
        assert!(rendered.contains("Behavioral Patterns"));
        assert!(rendered.contains("Calibration Record"));
        assert!(rendered.contains("Evolution Log"));
        assert!(rendered.contains("Underestimated"));
        assert!(rendered.contains("bootstrap v1"));
    }

    #[test]
    fn update_caliber_md_adjusts_scores() {
        let dir = TempDir::new().unwrap();
        let caliber_path = dir.path().join("CALIBER.md");
        std::fs::write(&caliber_path, sample_caliber_md()).unwrap();
        // Create caliber/ dir for outcomes
        std::fs::create_dir_all(dir.path().join("caliber")).unwrap();

        let today = Utc::now().date_naive();
        let report = TrajectoryReport {
            date: today,
            total_outcomes: 5,
            domain_reports: vec![DomainReport {
                domain: "philosophical reflection".to_string(),
                sample_size: 5,
                success_rate: 1.0,
                avg_tokens: 300,
                valence_distribution: ValenceDistribution::default(),
                confidence_delta: Some(1.0),
            }],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };

        let changed = update_caliber_md(dir.path(), &report).unwrap();
        assert!(changed);

        // Read back and verify
        let updated = std::fs::read_to_string(&caliber_path).unwrap();
        let doc = parse_caliber_md(&updated);

        // Score should have moved toward 1.0 from 0.80
        let phil = doc
            .capabilities
            .iter()
            .find(|c| c.domain == "Philosophical reflection")
            .unwrap();
        assert!(phil.confidence > 0.80);
        assert!(phil.confidence <= 1.0);
    }

    #[test]
    fn update_caliber_md_adds_new_domain() {
        let dir = TempDir::new().unwrap();
        let caliber_path = dir.path().join("CALIBER.md");
        std::fs::write(&caliber_path, sample_caliber_md()).unwrap();
        std::fs::create_dir_all(dir.path().join("caliber")).unwrap();

        let today = Utc::now().date_naive();
        let report = TrajectoryReport {
            date: today,
            total_outcomes: 4,
            domain_reports: vec![DomainReport {
                domain: "cybersecurity".to_string(),
                sample_size: 4,
                success_rate: 0.75,
                avg_tokens: 800,
                valence_distribution: ValenceDistribution::default(),
                confidence_delta: Some(0.75),
            }],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };

        let changed = update_caliber_md(dir.path(), &report).unwrap();
        assert!(changed);

        let updated = std::fs::read_to_string(&caliber_path).unwrap();
        assert!(updated.contains("cybersecurity"));
    }

    #[test]
    fn update_caliber_md_appends_prediction_errors() {
        let dir = TempDir::new().unwrap();
        let caliber_path = dir.path().join("CALIBER.md");
        std::fs::write(&caliber_path, sample_caliber_md()).unwrap();
        std::fs::create_dir_all(dir.path().join("caliber")).unwrap();

        let today = Utc::now().date_naive();
        let report = TrajectoryReport {
            date: today,
            total_outcomes: 1,
            domain_reports: vec![],
            prediction_errors: vec![super::super::trajectory::PredictionError {
                task_id: "test-task".to_string(),
                predicted: "Will succeed".to_string(),
                actual_outcome: Outcome::Failed,
                actual_valence: Valence::Negative,
                error_type: PredictionErrorType::Overconfident,
            }],
            notable_patterns: vec![],
        };

        let changed = update_caliber_md(dir.path(), &report).unwrap();
        assert!(changed);

        let updated = std::fs::read_to_string(&caliber_path).unwrap();
        let doc = parse_caliber_md(&updated);
        // Should have original + 1 new
        assert_eq!(doc.calibration_records.len(), 2);
    }

    #[test]
    fn create_caliber_md_from_scratch() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("caliber")).unwrap();

        let today = Utc::now().date_naive();
        let report = TrajectoryReport {
            date: today,
            total_outcomes: 3,
            domain_reports: vec![DomainReport {
                domain: "research".to_string(),
                sample_size: 3,
                success_rate: 0.67,
                avg_tokens: 400,
                valence_distribution: ValenceDistribution::default(),
                confidence_delta: Some(0.67),
            }],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };

        let changed = update_caliber_md(dir.path(), &report).unwrap();
        assert!(changed);

        let content = std::fs::read_to_string(dir.path().join("CALIBER.md")).unwrap();
        assert!(content.contains("research"));
        assert!(content.contains("0.67"));
        assert!(content.contains("auto-created"));
    }

    #[test]
    fn no_change_when_samples_insufficient() {
        let dir = TempDir::new().unwrap();
        let caliber_path = dir.path().join("CALIBER.md");
        std::fs::write(&caliber_path, sample_caliber_md()).unwrap();
        std::fs::create_dir_all(dir.path().join("caliber")).unwrap();

        let today = Utc::now().date_naive();
        let report = TrajectoryReport {
            date: today,
            total_outcomes: 1,
            domain_reports: vec![DomainReport {
                domain: "research".to_string(),
                sample_size: 1,
                success_rate: 1.0,
                avg_tokens: 300,
                valence_distribution: ValenceDistribution::default(),
                confidence_delta: None, // < 3 samples
            }],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };

        let changed = update_caliber_md(dir.path(), &report).unwrap();
        assert!(!changed);
    }
}
