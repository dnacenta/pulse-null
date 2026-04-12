//! Graph context injection — query the knowledge graph for entities relevant
//! to the current conversation and format them for the system prompt.

use std::path::Path;

/// Build a graph context block from the knowledge graph.
///
/// Queries the graph for entities semantically similar to `query_text`,
/// formats top results, and caps at `max_tokens`.
/// Returns `None` if the graph is unavailable or yields no results.
pub fn build_context_block(
    root_dir: &Path,
    query_text: &str,
    max_tokens: usize,
    _config: &crate::config::GraphConfig,
) -> Option<String> {
    if query_text.trim().is_empty() {
        return None;
    }

    let graph_dir = root_dir.join("memory").join("graph");

    // Run the graph query in a blocking context
    let rt = tokio::runtime::Runtime::new().ok()?;
    let result = rt.block_on(async {
        let gm = recall_echo::graph::GraphMemory::open(&graph_dir)
            .await
            .ok()?;

        let results = gm.search(query_text, 5).await.ok()?;
        if results.is_empty() {
            return None;
        }

        let mut block = String::from("<graph-context>\nRelevant knowledge from your graph:\n\n");
        let mut token_count = 15usize;

        for sr in &results {
            let e = &sr.entity;
            let entry = format!(
                "- **{}** ({}) [{:.0}%] — {}\n",
                e.name,
                e.entity_type,
                sr.score * 100.0,
                e.abstract_text,
            );

            let entry_tokens = entry.len() / 4;
            if token_count + entry_tokens > max_tokens {
                break;
            }
            block.push_str(&entry);
            token_count += entry_tokens;
        }

        let shown = block.matches("- **").count();
        let remaining = results.len().saturating_sub(shown);
        if remaining > 0 {
            block.push_str(&format!(
                "\n[{remaining} more entities available via graph_query tool]\n"
            ));
        }

        block.push_str("</graph-context>");
        Some(block)
    });

    result
}
