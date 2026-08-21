//! Local embeddings via fastembed (ONNX). This is the one crate that touches
//! the model cache — and, on the first run only, the network to download the
//! model. wikipethia-core stays free of both; it just consumes the vectors.

use std::sync::Mutex;

use wikipethia_core::{CoreError, Embedder};
use std::path::PathBuf;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: usize = 384;

pub struct FastEmbedder {
    model: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    /// Loads BGE-small-en-v1.5, downloading it on first use.
    ///
    /// The cache directory is resolved explicitly because fastembed's default
    /// is the **relative** `.fastembed_cache`, which is wrong the moment this
    /// is an installed tool rather than something run from one checkout: every
    /// working directory gets its own 128MB copy, and offline it simply fails.
    /// It bit the README's own `claude mcp add` example — the client launches
    /// the server with the *project's* cwd, so each project re-downloaded the
    /// model.
    ///
    /// Order: `FASTEMBED_CACHE_DIR` (fastembed's own variable, so anyone
    /// already setting it is unaffected), then a `.fastembed_cache` that
    /// already exists in the working directory (so an existing checkout keeps
    /// its download), then `~/.cache/wikipethia/fastembed`.
    pub fn new() -> Result<Self, CoreError> {
        let options = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(true)
            .with_cache_dir(cache_dir());
        let model = TextEmbedding::try_new(options).map_err(|e| CoreError::Embed(e.to_string()))?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

/// Where the model lives. See [`FastEmbedder::new`].
fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FASTEMBED_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    let local = PathBuf::from(".fastembed_cache");
    if local.is_dir() {
        return local;
    }
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home).join(".cache/wikipethia/fastembed"),
        // No home to put it in; the relative default is no worse than failing.
        None => local,
    }
}

impl Embedder for FastEmbedder {
    fn id(&self) -> &str {
        MODEL_ID
    }

    fn dimension(&self) -> usize {
        DIM
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        let mut model = self.model.lock().expect("embedder mutex poisoned");
        model
            .embed(texts, None)
            .map_err(|e| CoreError::Embed(e.to_string()))
    }

    // Queries embed unprefixed, same as passages. BGE v1.5 documents a query
    // prefix ("Represent this sentence for searching relevant passages: ")
    // but on this corpus it measurably hurt: eval-question cosines dropped
    // ~0.05 across the board and two expected posts fell out of the vector
    // top-30. Symmetric embedding is the empirically better default here.
}
