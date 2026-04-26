//! Graph query tool — gives entities the ability to search their knowledge graph.

use std::path::PathBuf;

use super::{Tool, ToolError, ToolResult};

pub struct GraphQueryTool {
    entity_root: PathBuf,
    graph_dir: PathBuf,
}

impl GraphQueryTool {
    pub fn new(entity_root: PathBuf) -> Self {
        let graph_dir = entity_root.join("memory").join("graph");
        Self {
            entity_root,
            graph_dir,
        }
    }
}

impl Tool for GraphQueryTool {
    fn name(&self) -> &str {
        "graph_query"
    }

    fn description(&self) -> &str {
        "Search your knowledge graph for entities, relationships, and past conversations. \
        Modes: 'search' (semantic search), 'entity' (lookup by name), \
        'relationships' (find connections), 'episodes' (search conversation history), \
        'stats' (graph overview), 'pipeline' (pipeline document stats)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query or entity name (not needed for 'stats' or 'pipeline' modes)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["search", "entity", "relationships", "episodes", "stats", "pipeline"],
                    "description": "Query mode: search (semantic), entity (by name), relationships (connections), episodes (conversations), stats (overview), pipeline (document stats)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return (default: 10)"
                }
            },
            "required": ["mode"]
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let graph_dir = self.graph_dir.clone();
        let entity_root = self.entity_root.clone();
        // Capture correlation_id from task-local before crossing the
        // spawn_blocking boundary (task-locals don't propagate through
        // the nested runtime). See utility-feedback-loop-spec.md.
        let correlation_id = crate::task_context::current();

        Box::pin(async move {
            let mode = input["mode"]
                .as_str()
                .ok_or_else(|| ToolError::ExecutionFailed("Missing 'mode' parameter".into()))?
                .to_string();

            let query = input["query"].as_str().unwrap_or("").to_string();
            let limit = input["limit"].as_u64().unwrap_or(10) as usize;

            if !graph_dir.exists() {
                return Err(ToolError::NotFound(
                    "Knowledge graph not initialized. No graph/ directory found.".into(),
                ));
            }

            // SurrealDB types aren't Send, so use spawn_blocking + nested runtime
            let result = tokio::task::spawn_blocking(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => return Err(format!("Failed to create runtime: {}", e)),
                };
                rt.block_on(async {
                    let gm = recall_echo::graph::GraphMemory::open(&graph_dir)
                        .await
                        .map_err(|e| format!("Failed to open graph: {}", e))?;

                    match mode.as_str() {
                        "search" => execute_search(&gm, &query, limit).await,
                        "entity" => execute_entity_lookup(&gm, &query).await,
                        "relationships" => execute_relationships(&gm, &query).await,
                        "episodes" => execute_episode_search(&gm, &query, limit).await,
                        "stats" => execute_stats(&gm).await,
                        "pipeline" => execute_pipeline_stats(&gm).await,
                        other => Err(format!(
                            "Unknown mode: '{}'. Valid: search, entity, relationships, episodes, stats, pipeline",
                            other
                        )),
                    }
                })
            })
            .await;

            match result {
                Ok(Ok((output, retrieved_ids))) => {
                    // Emit retrieval manifest for the utility feedback loop.
                    // Best-effort, no-op for non-retrieval modes (empty ids).
                    crate::graph_feedback::emit_manifest(
                        &entity_root,
                        correlation_id.as_deref(),
                        &retrieved_ids,
                    );
                    Ok(output)
                }
                Ok(Err(e)) => Err(ToolError::ExecutionFailed(e)),
                Err(e) => Err(ToolError::ExecutionFailed(format!("Task panicked: {}", e))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Mode implementations
// ---------------------------------------------------------------------------
// Each helper returns (formatted_output, retrieved_entity_ids). The IDs are
// fed to graph_feedback::emit_manifest for the utility feedback loop. Modes
// that don't surface retrievable entities (stats, pipeline, relationships)
// return an empty Vec.

use recall_echo::graph::GraphMemory;

async fn execute_search(
    gm: &GraphMemory,
    query: &str,
    limit: usize,
) -> Result<(String, Vec<String>), String> {
    if query.is_empty() {
        return Err("'query' is required for search mode".into());
    }

    let results = gm
        .search(query, limit)
        .await
        .map_err(|e| format!("Search failed: {}", e))?;

    if results.is_empty() {
        return Ok((format!("No results found for: {}", query), Vec::new()));
    }

    let retrieved_ids: Vec<String> = results.iter().map(|r| r.entity.id_string()).collect();

    let mut output = format!("Found {} result(s) for \"{}\":\n\n", results.len(), query);
    for (i, r) in results.iter().enumerate() {
        output.push_str(&format!(
            "{}. **{}** ({})\n   {}\n   Score: {:.2}\n\n",
            i + 1,
            r.entity.name,
            r.entity.entity_type,
            r.entity.abstract_text,
            r.score,
        ));
    }
    Ok((output, retrieved_ids))
}

async fn execute_entity_lookup(
    gm: &GraphMemory,
    name: &str,
) -> Result<(String, Vec<String>), String> {
    if name.is_empty() {
        return Err("'query' (entity name) is required for entity mode".into());
    }

    let entity = gm
        .get_entity(name)
        .await
        .map_err(|e| format!("Lookup failed: {}", e))?;

    match entity {
        Some(e) => {
            let retrieved_ids = vec![e.id_string()];
            let mut output = format!(
                "**{}** ({})\n\nAbstract: {}\n\nOverview: {}\n",
                e.name, e.entity_type, e.abstract_text, e.overview,
            );
            if let Some(ref content) = e.content {
                let display = if content.len() > crate::utils::CONTENT_TRUNCATE_LEN {
                    format!(
                        "{}...",
                        crate::utils::safe_truncate(content, crate::utils::CONTENT_TRUNCATE_LEN)
                    )
                } else {
                    content.clone()
                };
                output.push_str(&format!("\nContent:\n{}\n", display));
            }
            if let Some(ref attrs) = e.attributes {
                output.push_str(&format!("\nAttributes: {}\n", attrs));
            }
            Ok((output, retrieved_ids))
        }
        None => Ok((format!("No entity found with name: {}", name), Vec::new())),
    }
}

async fn execute_relationships(
    gm: &GraphMemory,
    name: &str,
) -> Result<(String, Vec<String>), String> {
    if name.is_empty() {
        return Err("'query' (entity name) is required for relationships mode".into());
    }

    let rels = gm
        .get_relationships(name, recall_echo::graph::types::Direction::Both)
        .await
        .map_err(|e| format!("Relationship query failed: {}", e))?;

    if rels.is_empty() {
        return Ok((format!("No relationships found for: {}", name), Vec::new()));
    }

    let mut output = format!("{} relationship(s) for \"{}\":\n\n", rels.len(), name);
    for r in &rels {
        let from = serde_json::to_string(&r.from_id).unwrap_or_default();
        let to = serde_json::to_string(&r.to_id).unwrap_or_default();
        output.push_str(&format!("- {} —[{}]→ {}\n", from, r.rel_type, to,));
        if let Some(ref desc) = r.description {
            output.push_str(&format!("  {}\n", desc));
        }
    }
    // Relationships mode surfaces edges, not retrievable entity records.
    Ok((output, Vec::new()))
}

async fn execute_episode_search(
    gm: &GraphMemory,
    query: &str,
    limit: usize,
) -> Result<(String, Vec<String>), String> {
    if query.is_empty() {
        return Err("'query' is required for episodes mode".into());
    }

    let results = gm
        .search_episodes(query, limit)
        .await
        .map_err(|e| format!("Episode search failed: {}", e))?;

    if results.is_empty() {
        return Ok((format!("No episodes found matching: {}", query), Vec::new()));
    }

    let retrieved_ids: Vec<String> = results
        .iter()
        .map(|r| match &r.episode.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();

    let mut output = format!(
        "Found {} episode(s) matching \"{}\":\n\n",
        results.len(),
        query
    );
    for (i, r) in results.iter().enumerate() {
        let log_info = r
            .episode
            .log_number
            .map(|n| format!(" (log #{})", n))
            .unwrap_or_default();
        output.push_str(&format!(
            "{}. Session: {}{}\n   {}\n   Score: {:.2}\n\n",
            i + 1,
            r.episode.session_id,
            log_info,
            r.episode.abstract_text,
            r.score,
        ));
    }
    Ok((output, retrieved_ids))
}

async fn execute_stats(gm: &GraphMemory) -> Result<(String, Vec<String>), String> {
    let stats = gm
        .stats()
        .await
        .map_err(|e| format!("Stats query failed: {}", e))?;

    let mut output = format!(
        "Knowledge Graph Overview:\n\n\
        Entities: {}\n\
        Relationships: {}\n\
        Episodes: {}\n\n",
        stats.entity_count, stats.relationship_count, stats.episode_count,
    );

    if !stats.entity_type_counts.is_empty() {
        output.push_str("Entity types:\n");
        let mut types: Vec<_> = stats.entity_type_counts.iter().collect();
        types.sort_by(|a, b| b.1.cmp(a.1));
        for (type_name, count) in types {
            output.push_str(&format!("  {}: {}\n", type_name, count));
        }
    }
    Ok((output, Vec::new()))
}

async fn execute_pipeline_stats(gm: &GraphMemory) -> Result<(String, Vec<String>), String> {
    let stats = gm
        .pipeline_stats(7)
        .await
        .map_err(|e| format!("Pipeline stats failed: {}", e))?;

    let mut output = format!(
        "Pipeline Graph Stats:\n\nTotal entities: {}\n",
        stats.total_entities
    );

    if let Some(ref last) = stats.last_movement {
        output.push_str(&format!("Last movement: {}\n", last));
    }

    if !stats.by_stage.is_empty() {
        output.push_str("\nBy stage:\n");
        for (stage, status_counts) in &stats.by_stage {
            let total: u64 = status_counts.values().sum();
            output.push_str(&format!("  {}: {} total", stage, total));
            let details: Vec<String> = status_counts
                .iter()
                .map(|(s, c)| format!("{}: {}", s, c))
                .collect();
            if !details.is_empty() {
                output.push_str(&format!(" ({})", details.join(", ")));
            }
            output.push('\n');
        }
    }

    if !stats.stale_thoughts.is_empty() {
        output.push_str(&format!(
            "\nStale thoughts ({}): ",
            stats.stale_thoughts.len()
        ));
        let names: Vec<&str> = stats
            .stale_thoughts
            .iter()
            .take(5)
            .map(|e| e.name.as_str())
            .collect();
        output.push_str(&names.join(", "));
        output.push('\n');
    }

    if !stats.stale_questions.is_empty() {
        output.push_str(&format!(
            "Stale questions ({}): ",
            stats.stale_questions.len()
        ));
        let names: Vec<&str> = stats
            .stale_questions
            .iter()
            .take(5)
            .map(|e| e.name.as_str())
            .collect();
        output.push_str(&names.join(", "));
        output.push('\n');
    }

    Ok((output, Vec::new()))
}
