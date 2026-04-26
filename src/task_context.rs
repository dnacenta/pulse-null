//! Task-local correlation key for outcome → retrieval manifest matching.
//!
//! When the LLM tool loop runs, retrievals via `graph_query` need to be
//! correlated with the eventual outcome record. The runner / intent /
//! chat handlers establish the scope; the tool reads the value before
//! crossing the spawn_blocking boundary into nested runtimes.
//!
//! See: `utility-feedback-loop-spec.md` (Component 1).

tokio::task_local! {
    static CORRELATION_ID: Option<String>;
}

/// Run an async block with `id` available to the `graph_query` tool.
/// `id` is typically the task ID for scheduled tasks / intents, or the
/// session key for chat sessions. Pass `None` if there is no correlation
/// key — the tool will skip manifest emission for that span.
pub async fn scope<F, R>(id: Option<String>, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    CORRELATION_ID.scope(id, fut).await
}

/// Read the current correlation ID. Returns `None` if not inside a `scope`.
pub fn current() -> Option<String> {
    CORRELATION_ID.try_with(|t| t.clone()).ok().flatten()
}
