use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ::fastembed::{
    EmbeddingModel, InitOptionsUserDefined, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use anyhow::{Context, Result, anyhow};

use super::Embedder;
use super::download::{self, Download};
use super::threads::embed_threads;
use crate::config::DEFAULT_MODEL_DOWNLOAD_TIMEOUT_SECS;

const MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;
/// The tokenizer files fastembed reads, small ones first: the first file downloaded fixes the
/// commit of the others.
const TOKENIZER_FILES: [&str; 4] = [
    "config.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
    "tokenizer.json",
];
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
    /// Loads the model from `cache_dir`, downloading it (about 130 MB) from `HF_ENDPOINT` when
    /// it is missing, within the default deadline.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let deadline = Duration::from_secs(DEFAULT_MODEL_DOWNLOAD_TIMEOUT_SECS);
        Self::with_download(cache_dir, &Download::new(deadline), embed_threads(None))
    }

    /// Loads the model from `cache_dir` (`$HF_HOME` instead, when that is set), downloading it
    /// as `download` says when it is missing.
    /// The document instance uses `threads` ONNX Runtime threads (see [`embed_threads`]).
    pub fn with_download(cache_dir: &Path, download: &Download, threads: usize) -> Result<Self> {
        Self::from_snapshot(&fetch_snapshot(cache_dir, download)?, threads)
    }

    /// Loads both instances from the model files in `snapshot`, without the network.
    fn from_snapshot(snapshot: &Path, threads: usize) -> Result<Self> {
        Ok(Self {
            documents: Mutex::new(load(snapshot, Some(threads))?),
            queries: Mutex::new(load(snapshot, Some(QUERY_THREADS))?),
        })
    }
}

/// The snapshot directory with the model files, downloading them as `download` says when they
/// are missing from `cache_dir` (`$HF_HOME` instead, when that is set).
fn fetch_snapshot(cache_dir: &Path, download: &Download) -> Result<PathBuf> {
    // fastembed's own loader let HF_HOME replace the cache directory, and the docs say so.
    let cache_dir = std::env::var_os("HF_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| cache_dir.to_path_buf(), PathBuf::from);
    let info = TextEmbedding::get_model_info(&MODEL)?;
    let mut files = TOKENIZER_FILES.to_vec();
    files.push(&info.model_file);
    download::fetch(&info.model_code, &files, &cache_dir, download)
}

/// Makes sure the model is in `cache_dir` (`$HF_HOME` instead, when that is set): downloads it
/// when it is missing, then loads it once to prove the files work. Returns the snapshot
/// directory. The image build runs this so the model is baked in.
pub fn fetch_model(cache_dir: &Path, download: &Download) -> Result<PathBuf> {
    let snapshot = fetch_snapshot(cache_dir, download)?;
    FastEmbedder::from_snapshot(&snapshot, embed_threads(None))?;
    Ok(snapshot)
}

/// One instance of the model, from the files in `snapshot`, with the settings
/// `TextEmbedding::try_new` uses for it. fastembed never touches the network this way.
/// `threads` caps its ONNX Runtime threads (`None`: the runtime's default, one per core). The
/// document instance gets the container-aware count from [`embed_threads`].
fn load(snapshot: &Path, threads: Option<usize>) -> Result<TextEmbedding> {
    let read = |file: &str| {
        let path = snapshot.join(file);
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    };
    let info = TextEmbedding::get_model_info(&MODEL)?;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let mut model = UserDefinedEmbeddingModel::new(read(&info.model_file)?, tokenizer_files)
        .with_quantization(TextEmbedding::get_quantization_mode(&MODEL));
    model.pooling = TextEmbedding::get_default_pooling_method(&MODEL);
    model.output_key = info.output_key.clone();
    let mut options = InitOptionsUserDefined::new();
    if let Some(threads) = threads {
        options = options.with_intra_threads(threads);
    }
    TextEmbedding::try_new_from_user_defined(model, options)
        .map_err(|e| anyhow!("loading bge-small-en-v1.5 from {}: {e}", snapshot.display()))
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

    /// The load goes through the download with its deadline: a hub that accepts the
    /// connection and then says nothing stops it in time, with the URL in the error.
    #[test]
    fn a_silent_hub_stops_the_load_at_the_deadline() {
        // HF_HOME replaces the cache directory, and may already hold the model.
        if std::env::var_os("HF_HOME").is_some() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                held.push(stream);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let download = Download::with_endpoint(&base, Duration::from_millis(500));
        let started = std::time::Instant::now();
        let err = FastEmbedder::with_download(dir.path(), &download, 1)
            .err()
            .expect("the load fails")
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(3), "{err}");
        assert!(
            err.contains(&format!(
                "{base}/Xenova/bge-small-en-v1.5/resolve/main/config.json"
            )),
            "{err}"
        );
    }

    /// `fetch_model` finds the model in an existing cache without the network, then loads it:
    /// the cache here holds files that are not an ONNX model, so the load fails and names the
    /// snapshot it read. A real download and load is covered by `fastembed_smoke`.
    #[test]
    fn fetch_model_reads_the_cache_and_loads_from_the_snapshot() {
        // HF_HOME replaces the cache directory, and may already hold the model.
        if std::env::var_os("HF_HOME").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let info = TextEmbedding::get_model_info(&MODEL).unwrap();
        let repo_dir = dir
            .path()
            .join(format!("models--{}", info.model_code.replace('/', "--")));
        let snapshot = repo_dir.join("snapshots/abc123");
        let mut files = TOKENIZER_FILES.to_vec();
        files.push(&info.model_file);
        for file in files {
            let path = snapshot.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "not a model").unwrap();
        }
        std::fs::create_dir_all(repo_dir.join("refs")).unwrap();
        std::fs::write(repo_dir.join("refs/main"), "abc123").unwrap();
        // Nothing listens on port 9: the files in the cache must be enough to get that far.
        let download = Download::with_endpoint("http://127.0.0.1:9", Duration::from_secs(1));
        let err = fetch_model(dir.path(), &download).unwrap_err().to_string();
        let want = format!("loading bge-small-en-v1.5 from {}", snapshot.display());
        assert!(err.contains(&want), "{err}");
    }

    /// Downloads the model (about 130 MB) into a temp dir. Run with
    /// `cargo test -- --ignored fastembed_smoke`.
    #[test]
    #[ignore = "downloads the bge-small-en-v1.5 model"]
    fn fastembed_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let download = Download::new(Duration::from_secs(DEFAULT_MODEL_DOWNLOAD_TIMEOUT_SECS));
        let snapshot = fetch_model(dir.path(), &download).unwrap();
        assert!(snapshot.join("tokenizer.json").exists());
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
