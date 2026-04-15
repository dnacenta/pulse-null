//! Graph context — cached stats for prompt awareness + future async retrieval.
//!
//! The prompt builder runs in a sync (spawn_blocking) context and cannot do async
//! graph queries. Instead, we cache graph stats to a JSON file during the async
//! boot sequence, then read the cached file synchronously in the prompt builder.

use std::path::Path;

const STATS_CACHE_FILE: &str = "memory/graph/stats-cache.json";

/// Cache graph stats to a JSON file. Called during async boot sequence.
pub async fn cache_graph_stats(root_dir: &Path) {
    let graph_dir = root_dir.join("memory").join("graph");

    match recall_echo::graph::GraphMemory::open(&graph_dir).await {
        Ok(gm) => match gm.stats().await {
            Ok(stats) => {
                let cache = serde_json::json!({
                    "entity_count": stats.entity_count,
                    "relationship_count": stats.relationship_count,
                    "episode_count": stats.episode_count,
                });
                let cache_path = root_dir.join(STATS_CACHE_FILE);
                if let Err(e) = std::fs::write(&cache_path, cache.to_string()) {
                    tracing::warn!("Failed to cache graph stats: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to query graph stats: {e}"),
        },
        Err(e) => tracing::warn!("Failed to open graph for stats cache: {e}"),
    }
}

/// Read cached graph stats synchronously. Returns a formatted awareness hint
/// for the system prompt, or None if stats aren't available.
pub fn graph_awareness_hint(root_dir: &Path) -> Option<String> {
    let cache_path = root_dir.join(STATS_CACHE_FILE);
    let content = std::fs::read_to_string(&cache_path).ok()?;
    let stats: serde_json::Value = serde_json::from_str(&content).ok()?;

    let entities = stats["entity_count"].as_u64().unwrap_or(0);
    let relationships = stats["relationship_count"].as_u64().unwrap_or(0);
    let episodes = stats["episode_count"].as_u64().unwrap_or(0);

    if entities == 0 && episodes == 0 {
        return None;
    }

    Some(format!(
        "<graph-awareness>\n\
        Your knowledge graph contains {entities} entities, {relationships} relationships, \
        and {episodes} conversation episodes. Use the graph_query tool when:\n\
        - You need to recall past conversations or decisions\n\
        - You want relationships between people, projects, or concepts\n\
        - You're looking for specific knowledge on a topic\n\
        Available modes: search, entity, relationships, episodes, stats, pipeline\n\
        </graph-awareness>"
    ))
}
