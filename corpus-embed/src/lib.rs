//! Local embeddings via fastembed (ONNX). This is the one crate that touches
//! the model cache — and, on the first run only, the network to download the
//! model. corpus-core stays free of both; it just consumes the vectors.

use std::sync::Mutex;

use corpus_core::{CoreError, Embedder};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: usize = 384;

pub struct FastEmbedder {
    model: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    /// Loads BGE-small-en-v1.5, downloading it to the fastembed cache
    /// (`.fastembed_cache/`, or `FASTEMBED_CACHE_DIR`) on first use.
    pub fn new() -> Result<Self, CoreError> {
        let options =
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(true);
        let model = TextEmbedding::try_new(options).map_err(|e| CoreError::Embed(e.to_string()))?;
        Ok(Self {
            model: Mutex::new(model),
        })
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
