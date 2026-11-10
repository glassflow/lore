//! Text embedders. `fastembed` runs bge-small-en-v1.5 locally on CPU; `hash` is a deterministic
//! stand-in so tests never download a model.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, EmbedderKind};

pub mod download;
pub mod fastembed;
pub mod hash;

pub use self::download::Download;
pub use self::fastembed::FastEmbedder;
pub use self::hash::HashEmbedder;

/// Dimension of every embedding (bge-small-en-v1.5, and the hash embedder to match).
pub const DIM: usize = 384;

pub trait Embedder: Send + Sync {
    /// `"fastembed:bge-small-en-v1.5"` or `"hash"`.
    fn name(&self) -> &str;
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
}

/// Builds the embedder chosen by `LORE_EMBEDDER`. For fastembed this creates
/// `<data_dir>/models` and loads the model, downloading it on first use within
/// `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS`.
pub fn from_config(cfg: &Config) -> Result<Arc<dyn Embedder>> {
    match cfg.embedder {
        EmbedderKind::Hash => Ok(Arc::new(HashEmbedder)),
        EmbedderKind::Fastembed => {
            let dir = cfg.data_dir.join("models");
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating model directory {}", dir.display()))?;
            let download = Download::new(Duration::from_secs(cfg.model_download_timeout_secs));
            Ok(Arc::new(FastEmbedder::with_download(&dir, &download)?))
        }
    }
}

/// Runs `embed_documents` on the blocking pool, so a CPU-bound batch does not stall the runtime.
pub async fn embed_documents_async(
    e: Arc<dyn Embedder>,
    texts: Vec<String>,
) -> Result<Vec<Vec<f32>>> {
    tokio::task::spawn_blocking(move || e.embed_documents(&texts))
        .await
        .context("embedding task failed")?
}

/// Runs `embed_query` on the blocking pool.
pub async fn embed_query_async(e: Arc<dyn Embedder>, text: String) -> Result<Vec<f32>> {
    tokio::task::spawn_blocking(move || e.embed_query(&text))
        .await
        .context("embedding task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_wrappers_match_the_sync_calls() {
        let e: Arc<dyn Embedder> = Arc::new(HashEmbedder);
        let docs = embed_documents_async(e.clone(), vec!["plan limits".into(), "seats".into()])
            .await
            .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0], e.embed_query("plan limits").unwrap());
        let q = embed_query_async(e.clone(), "seats".into()).await.unwrap();
        assert_eq!(q, docs[1]);
    }

    #[test]
    fn from_config_builds_the_hash_embedder() {
        let cfg =
            Config::from_lookup(|k| (k == "LORE_EMBEDDER").then(|| "hash".to_string())).unwrap();
        assert_eq!(from_config(&cfg).unwrap().name(), "hash");
    }
}
