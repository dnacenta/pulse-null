/// A capability entry in the generated awareness manifest.
///
/// Pure data struct — rendering is handled by [`render`] and [`render_section`].
pub struct Capability {
    pub name: String,
    pub what: String,
    pub why: String,
    pub how: String,
    pub constraints: Option<String>,
}

/// Render a single capability as a markdown section.
pub fn render(cap: &Capability) -> String {
    let mut out = format!(
        "### {}\n**What:** {}\n**Why:** {}\n**How:** {}",
        cap.name, cap.what, cap.why, cap.how
    );
    if let Some(ref c) = cap.constraints {
        out.push_str(&format!("\n**Constraints:** {}", c));
    }
    out
}

/// Render a list of capabilities as the "## Capabilities" section.
/// Returns None if the list is empty.
pub fn render_section(capabilities: &[Capability]) -> Option<String> {
    if capabilities.is_empty() {
        return None;
    }
    let rendered: Vec<String> = capabilities.iter().map(render).collect();
    Some(format!("## Capabilities\n\n{}", rendered.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_all_fields() {
        let cap = Capability {
            name: "TestCap".into(),
            what: "Does things".into(),
            why: "Because".into(),
            how: "Like this".into(),
            constraints: Some("Max 10".into()),
        };
        let rendered = render(&cap);
        assert!(rendered.contains("### TestCap"));
        assert!(rendered.contains("**What:** Does things"));
        assert!(rendered.contains("**Why:** Because"));
        assert!(rendered.contains("**How:** Like this"));
        assert!(rendered.contains("**Constraints:** Max 10"));
    }

    #[test]
    fn render_omits_constraints_when_none() {
        let cap = Capability {
            name: "NoCons".into(),
            what: "W".into(),
            why: "Y".into(),
            how: "H".into(),
            constraints: None,
        };
        let rendered = render(&cap);
        assert!(!rendered.contains("Constraints"));
    }

    #[test]
    fn render_section_returns_none_for_empty() {
        assert!(render_section(&[]).is_none());
    }

    #[test]
    fn render_section_wraps_with_header() {
        let caps = vec![Capability {
            name: "A".into(),
            what: "W".into(),
            why: "Y".into(),
            how: "H".into(),
            constraints: None,
        }];
        let section = render_section(&caps).unwrap();
        assert!(section.starts_with("## Capabilities"));
        assert!(section.contains("### A"));
    }

    #[test]
    fn render_section_separates_multiple_caps() {
        let caps = vec![
            Capability {
                name: "First".into(),
                what: "W".into(),
                why: "Y".into(),
                how: "H".into(),
                constraints: None,
            },
            Capability {
                name: "Second".into(),
                what: "W".into(),
                why: "Y".into(),
                how: "H".into(),
                constraints: None,
            },
        ];
        let section = render_section(&caps).unwrap();
        assert!(section.contains("### First"));
        assert!(section.contains("### Second"));
        // Capabilities should be separated by double newline
        assert!(section.contains("\n\n### Second"));
    }
}
