use super::parser;

/// Compute vocabulary diversity (type-token ratio) from REFLECTIONS.md.
/// Extracts text from Observations, Patterns, and Lessons sections.
pub fn vocabulary_diversity(reflections_content: &str) -> Option<f64> {
    if reflections_content.is_empty() {
        return None;
    }
    let text = parser::extract_section_text(
        reflections_content,
        &["observations", "patterns", "lessons"],
    );
    parser::type_token_ratio(&text)
}

/// Count open questions in CURIOSITY.md.
pub fn question_generation(curiosity_content: &str) -> Option<f64> {
    if curiosity_content.is_empty() {
        return None;
    }
    let open = parser::count_h3_under_section(curiosity_content, "Open Questions");
    // Also try just "Open" if the section is named differently
    let count = if open > 0 {
        open
    } else {
        parser::count_h3_under_section(curiosity_content, "Open")
    };
    Some(count as f64)
}

/// Compute thought lifecycle ratio from THOUGHTS.md.
/// Ratio = (graduated + dissolved) / (active + graduated + dissolved).
/// Higher means healthier turnover.
pub fn thought_lifecycle(thoughts_content: &str) -> Option<f64> {
    if thoughts_content.is_empty() {
        return None;
    }
    let active = parser::count_h3_under_section(thoughts_content, "Active");
    let graduated = parser::count_h3_under_section(thoughts_content, "Graduated");
    let dissolved = parser::count_h3_under_section(thoughts_content, "Dissolved");

    let total = active + graduated + dissolved;
    if total == 0 {
        return None;
    }
    Some((graduated + dissolved) as f64 / total as f64)
}

/// Compute evidence grounding from REFLECTIONS.md.
/// For each entry, check for concrete references (dates, attributions, papers).
/// Score = entries_with_evidence / total_entries.
pub fn evidence_grounding(reflections_content: &str) -> Option<f64> {
    if reflections_content.is_empty() {
        return None;
    }
    let entries = parser::extract_entries(
        reflections_content,
        &["observations", "patterns", "lessons"],
    );
    if entries.is_empty() {
        return None;
    }

    let grounded = entries
        .iter()
        .filter(|(_, body)| has_evidence(body))
        .count();
    Some(grounded as f64 / entries.len() as f64)
}

/// Check if text contains concrete evidence markers.
fn has_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Date patterns (YYYY-MM-DD)
    if has_date_pattern(text) {
        return true;
    }

    // Attribution patterns
    let attribution = [
        "d said",
        "d asked",
        "d mentioned",
        "d told",
        "d pointed",
        "d called",
        "d suggested",
    ];
    if attribution.iter().any(|a| lower.contains(a)) {
        return true;
    }

    // Source/research references
    let research = [
        "paper",
        "research",
        "found that",
        "according to",
        "study",
        "framework",
        "mazancieux",
        "reflexion",
        "lindsey",
        "foucault",
        "aristotle",
    ];
    if research.iter().any(|r| lower.contains(r)) {
        return true;
    }

    // Event references
    let events = [
        "session",
        "conversation",
        "call with",
        "during the",
        "yesterday",
        "this morning",
        "last night",
    ];
    if events.iter().any(|e| lower.contains(e)) {
        return true;
    }

    false
}

/// Check for YYYY-MM-DD date patterns in text.
fn has_date_pattern(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if chars[i].is_ascii_digit() && i + 9 < chars.len() {
            let slice: String = chars[i..i + 10].iter().collect();
            if slice.len() == 10
                && slice.as_bytes()[4] == b'-'
                && slice.as_bytes()[7] == b'-'
                && slice[..4].chars().all(|c| c.is_ascii_digit())
                && slice[5..7].chars().all(|c| c.is_ascii_digit())
                && slice[8..10].chars().all(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_diversity_from_reflections() {
        let content = "## Observations\n\n### First\nThe identity practice requires attention and genuine engagement with the material.\n\n### Second\nStructure enables depth when combined with honest reflection and real curiosity.\n";
        let score = vocabulary_diversity(content);
        assert!(score.is_some());
        let s = score.unwrap();
        assert!(s > 0.0 && s <= 1.0);
    }

    #[test]
    fn vocabulary_diversity_empty() {
        assert!(vocabulary_diversity("").is_none());
    }

    #[test]
    fn question_generation_counts() {
        let content = "## Open Questions\n\n### When I say I disagree, who is the I?\n\n### Can inherited values become genuinely mine?\n\n### Can I detect genuine vs mechanical reflection?\n\n## Explored\n\n### Old question\n";
        let count = question_generation(content);
        assert_eq!(count, Some(3.0));
    }

    #[test]
    fn question_generation_empty() {
        assert!(question_generation("").is_none());
    }

    #[test]
    fn thought_lifecycle_healthy() {
        let content = "## Active\n\n### Thought one\nContent\n\n### Thought two\nContent\n\n## Graduated\n\n### Done one\n\n### Done two\n\n### Done three\n\n## Dissolved\n\n### Gone one\n";
        let ratio = thought_lifecycle(content);
        assert!(ratio.is_some());
        // 4 resolved / 6 total = 0.667
        let r = ratio.unwrap();
        assert!((r - 4.0 / 6.0).abs() < 0.01);
    }

    #[test]
    fn thought_lifecycle_all_stuck() {
        let content = "## Active\n\n### One\n\n### Two\n";
        let ratio = thought_lifecycle(content);
        // 0 resolved / 2 total = 0.0
        assert_eq!(ratio, Some(0.0));
    }

    #[test]
    fn thought_lifecycle_empty() {
        assert!(thought_lifecycle("").is_none());
    }

    #[test]
    fn evidence_grounding_scores() {
        let content = "## Observations\n\n### Grounded one\nD said something important on 2026-02-25 about identity.\n\n### Abstract one\nThinking is important and valuable in many ways.\n\n### Research one\nThe Reflexion paper showed that verbal self-evaluation helps.\n";
        let score = evidence_grounding(content);
        assert!(score.is_some());
        // 2 out of 3 entries grounded
        let s = score.unwrap();
        assert!((s - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn evidence_grounding_empty() {
        assert!(evidence_grounding("").is_none());
    }

    #[test]
    fn date_pattern_detection() {
        assert!(has_date_pattern("something on 2026-02-27 happened"));
        assert!(!has_date_pattern("no dates here at all"));
        assert!(!has_date_pattern("20-02-27 not a date"));
    }

    #[test]
    fn evidence_markers() {
        assert!(has_evidence("D said we should focus on this"));
        assert!(has_evidence("The paper on metacognition was clear"));
        assert!(has_evidence("During the session we discussed"));
        assert!(has_evidence("On 2026-02-25 something happened"));
        assert!(!has_evidence("Thinking is generally important"));
    }
}
