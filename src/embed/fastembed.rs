use std::path::Path;
use std::sync::Mutex;

use ::fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use anyhow::{Result, anyhow};

use super::Embedder;

const BATCH_SIZE: usize = 64;
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
/// ONNX Runtime threads of the query instance. A query is one short text, which more threads
/// barely speed up, and fewer leave the cores to an index that may be running.
const QUERY_THREADS: usize = 2;

/// bge-small-en-v1.5 on CPU through the ONNX runtime, loaded twice: one instance embeds the
/// documents being indexed, the other embeds search queries. `TextEmbedding::embed` takes
/// `&mut self`, so each instance has its own mutex. With one shared instance a search waited for
/// the indexing batch in progress, and took seconds while a repo was being pushed.
///
/// Documents are embedded one batch per lock, so pushes that run at the same time (other repos,
/// other tenants) take turns instead of each waiting for a whole push.
pub struct FastEmbedder {
    documents: Mutex<TextEmbedding>,
    queries: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    /// Loads the model from `cache_dir`, downloading it (about 130 MB) when it is missing.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        Ok(Self {
            documents: Mutex::new(load(cache_dir, None)?),
            queries: Mutex::new(load(cache_dir, Some(QUERY_THREADS))?),
        })
    }
}

/// One instance of the model; `threads` caps its ONNX Runtime threads (`None`: the runtime's
/// default, one per core).
fn load(cache_dir: &Path, threads: Option<usize>) -> Result<TextEmbedding> {
    let mut options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
        .with_cache_dir(cache_dir.to_path_buf())
        .with_show_download_progress(false);
    if let Some(threads) = threads {
        options = options.with_intra_threads(threads);
    }
    TextEmbedding::try_new(options).map_err(|e| {
        anyhow!(
            "loading bge-small-en-v1.5 into {}: {e}",
            cache_dir.display()
        )
    })
}

/// Embeds `texts` (at most one batch) while holding `model`'s lock.
fn embed(model: &Mutex<TextEmbedding>, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let mut model = model
        .lock()
        .map_err(|_| anyhow!("embedding model lock poisoned"))?;
    model
        .embed(texts, Some(BATCH_SIZE))
        .map_err(|e| anyhow!("embedding failed: {e}"))
}

impl Embedder for FastEmbedder {
    fn name(&self) -> &str {
        "fastembed:bge-small-en-v1.5"
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH_SIZE) {
            vectors.extend(embed(&self.documents, batch)?);
        }
        Ok(vectors)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = vec![format!("{QUERY_PREFIX}{text}")];
        embed(&self.queries, &prefixed)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no vector"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::DIM;

    /// Downloads the model (about 130 MB) into a temp dir. Run with
    /// `cargo test -- --ignored fastembed_smoke`.
    #[test]
    #[ignore = "downloads the bge-small-en-v1.5 model"]
    fn fastembed_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let e = FastEmbedder::new(dir.path()).unwrap();
        assert_eq!(e.name(), "fastembed:bge-small-en-v1.5");
        let docs = e
            .embed_documents(&[
                "plan limits seats".to_string(),
                "limits for each plan".to_string(),
                "theme toggle button".to_string(),
            ])
            .unwrap();
        assert_eq!(docs.len(), 3);
        assert!(docs.iter().all(|v| v.len() == DIM));
        assert_eq!(e.embed_query("plan limits").unwrap().len(), DIM);
        assert!(e.embed_documents(&[]).unwrap().is_empty());

        // The two instances are the same model: a query and the same text embedded as a
        // document agree, whatever their thread counts.
        let query = e.embed_query("plan limits").unwrap();
        let as_document = e
            .embed_documents(&[format!("{QUERY_PREFIX}plan limits")])
            .unwrap()
            .remove(0);
        let cosine: f32 = query.iter().zip(&as_document).map(|(a, b)| a * b).sum();
        assert!(cosine > 0.9999, "{cosine}");

        // More than one batch comes back whole and in order.
        let many: Vec<String> = (0..150).map(|i| format!("text number {i}")).collect();
        let vectors = e.embed_documents(&many).unwrap();
        assert_eq!(vectors.len(), 150);
        assert_eq!(vectors[149], e.embed_documents(&many[149..]).unwrap()[0]);
    }

    /// A query does not wait for the documents being embedded. Downloads the model; run with
    /// `cargo test --release -- --ignored a_query_does_not_wait`.
    #[test]
    #[ignore = "downloads the bge-small-en-v1.5 model"]
    fn a_query_does_not_wait_for_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let e = std::sync::Arc::new(FastEmbedder::new(dir.path()).unwrap());
        let paragraph = "lore indexes code, documents and agent sessions for search. ".repeat(20);
        let docs: Vec<String> = (0..1280).map(|i| format!("{i} {paragraph}")).collect();
        let indexing = {
            let e = e.clone();
            std::thread::spawn(move || e.embed_documents(&docs).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(500));
        let started = std::time::Instant::now();
        e.embed_query("plan limits").unwrap();
        let waited = started.elapsed();
        assert!(
            !indexing.is_finished(),
            "the index ended first; give it more documents"
        );
        assert!(waited < std::time::Duration::from_secs(1), "{waited:?}");
        indexing.join().unwrap();
    }
}
