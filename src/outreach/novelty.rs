//! Gate 1's oracle — novelty against the record (PN-94, spec §2.3.1).
//!
//! The dominant failure of a self-triggered channel is restating: the entity
//! rediscovers something it already wrote down and sends it as news. So a
//! headline is compared against what is already on the record, and "already
//! on the record" means two things — every prior outreach message, and every
//! entry heading in the journal documents.
//!
//! Similarity is cosine distance over recall-echo's embedding path
//! (fastembed, BGE-Small-EN-v1.5, 384 dimensions) — the same model the
//! knowledge graph indexes episodes with, so "semantically matches" means
//! here exactly what it means in memory retrieval. The model cache is shared
//! with the graph's, so the first candidate on a machine that has already
//! ingested an episode pays nothing.
//!
//! Loading the ONNX runtime is slow and can fail (no cache, no network on
//! first ever use). Both are handled the same way: an `Err` from
//! [`NoveltyIndex::max_similarity`] means novelty is *unknown*, and the gate
//! reads unknown as a rejection. Under-firing is the recoverable direction.

use std::path::Path;

use recall_echo::graph::embed::{Embedder, LazyEmbedder};

use super::store::OutreachStore;
use super::NoveltyIndex;

/// Upper bound on corpus entries compared against one headline.
///
/// Bounds the per-candidate embedding cost. The newest entries are kept: an
/// eight-month-old journal heading is a weaker claim to "already said this"
/// than last week's outreach.
const MAX_CORPUS: usize = 400;

/// Journal documents whose entry headings form the second half of the corpus.
const JOURNAL_DOCS: &[&str] = &[
    "LEARNING.md",
    "THOUGHTS.md",
    "CURIOSITY.md",
    "REFLECTIONS.md",
    "PRAXIS.md",
    "FINDINGS.md",
];

/// Novelty backed by recall-echo's local embedding model.
pub struct EmbeddingNovelty {
    embedder: LazyEmbedder,
}

impl EmbeddingNovelty {
    /// Construction is free — the model loads on first actual comparison.
    #[must_use]
    pub fn new(root_dir: &Path) -> Self {
        Self {
            embedder: LazyEmbedder::new(&root_dir.join("memory").join("graph").join("models")),
        }
    }
}

impl NoveltyIndex for EmbeddingNovelty {
    fn max_similarity(&self, headline: &str, corpus: &[String]) -> Result<f64, String> {
        if corpus.is_empty() {
            // Nothing on the record yet, so nothing can be a restatement.
            return Ok(0.0);
        }

        let embedder = self.embedder.get().map_err(|e| e.to_string())?;
        let mut texts: Vec<&str> = Vec::with_capacity(corpus.len() + 1);
        texts.push(headline);
        texts.extend(corpus.iter().map(String::as_str));

        let vectors = embedder.embed(texts).map_err(|e| e.to_string())?;
        let (query, rest) = vectors
            .split_first()
            .ok_or_else(|| "embedder returned no vectors".to_string())?;
        if rest.len() != corpus.len() {
            return Err(format!(
                "embedder returned {} vectors for {} corpus entries",
                rest.len(),
                corpus.len()
            ));
        }

        Ok(rest
            .iter()
            .map(|entry| cosine_similarity(query, entry))
            .fold(0.0_f64, f64::max))
    }
}

/// Cosine similarity clamped to `[0, 1]`.
///
/// The clamp is not cosmetic: the gate compares against a `[0, 1]` threshold,
/// and a negative similarity (semantically opposed text) is maximal novelty,
/// not a value that should wrap round into a different meaning.
#[must_use]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
        norm_a += f64::from(*x) * f64::from(*x);
        norm_b += f64::from(*y) * f64::from(*y);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(0.0, 1.0)
}

/// Assemble the corpus a headline is judged against.
///
/// Prior outreach headlines first, then journal entry headings. Both are
/// short, declarative and roughly headline-shaped, which is the granularity
/// the comparison is meaningful at — embedding a whole 8 KB THOUGHTS.md
/// against one sentence measures topic overlap, not restatement.
#[must_use]
pub fn build_corpus(root_dir: &Path, store: &OutreachStore) -> Vec<String> {
    let mut corpus = store.sent_headlines();
    corpus.extend(journal_headings(root_dir));
    if corpus.len() > MAX_CORPUS {
        corpus.drain(..corpus.len() - MAX_CORPUS);
    }
    corpus
}

/// Entry headings from the journal documents.
fn journal_headings(root_dir: &Path) -> Vec<String> {
    let docs_dir = crate::scheduler::evaluator::resolve_docs_dir(root_dir);
    JOURNAL_DOCS
        .iter()
        .filter_map(|name| std::fs::read_to_string(docs_dir.join(name)).ok())
        .flat_map(|content| headings(&content))
        .collect()
}

/// Markdown headings in `content`, stripped of their `#` markers.
///
/// Level-1 headings are skipped: they are the document title
/// ("# LEARNING"), which every entry in the file would trivially match.
fn headings(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("##") {
                return None;
            }
            let text = trimmed.trim_start_matches('#').trim();
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::SalienceKind;
    use crate::outreach::test_support::sent;
    use chrono::Utc;
    use tempfile::TempDir;

    #[test]
    fn headings_skip_titles_and_prose() {
        let content = "\
# LEARNING

Some intro prose.

## Thread: the listener sheds during isolation

Body text that is not a heading.

### Sub-point about leases

##

##   Trailing spaces are trimmed
";
        assert_eq!(
            headings(content),
            vec![
                "Thread: the listener sheds during isolation",
                "Sub-point about leases",
                "Trailing spaces are trimmed",
            ]
        );
    }

    #[test]
    fn corpus_joins_prior_headlines_and_journal_headings() {
        let tmp = TempDir::new().unwrap();
        let journal = tmp.path().join("journal");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(
            journal.join("THOUGHTS.md"),
            "# THOUGHTS\n\n## A thought about caps\n",
        )
        .unwrap();

        let mut store = OutreachStore::default();
        store.record_sent(sent("m1", SalienceKind::Finding, Utc::now(), false));

        let corpus = build_corpus(tmp.path(), &store);
        assert!(corpus.contains(&"headline m1".to_string()));
        assert!(corpus.contains(&"A thought about caps".to_string()));
    }

    #[test]
    fn corpus_is_bounded() {
        let tmp = TempDir::new().unwrap();
        let mut store = OutreachStore::default();
        for i in 0..(MAX_CORPUS + 50) {
            store.record_sent(sent(
                &format!("m{i}"),
                SalienceKind::Finding,
                Utc::now(),
                false,
            ));
        }
        assert!(build_corpus(tmp.path(), &store).len() <= MAX_CORPUS);
    }

    #[test]
    fn corpus_on_a_fresh_entity_is_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(build_corpus(tmp.path(), &OutreachStore::default()).is_empty());
    }

    #[test]
    fn an_empty_corpus_is_maximal_novelty_without_loading_a_model() {
        // Must not touch the ONNX runtime: the first message an entity ever
        // sends cannot be a restatement of nothing.
        let tmp = TempDir::new().unwrap();
        let novelty = EmbeddingNovelty::new(tmp.path());
        assert_eq!(novelty.max_similarity("anything", &[]).unwrap(), 0.0);
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![0.3_f32, -0.4, 0.5];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_clamps_opposed_vectors_to_zero() {
        // Semantically opposed is maximally novel, not negatively similar.
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_of_mismatched_or_zero_vectors_is_zero() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}
